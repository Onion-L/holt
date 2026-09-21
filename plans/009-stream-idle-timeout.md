# Plan 009: Terminate provider streams that stop producing events

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 74d34da..HEAD -- crates/engine/src/agent.rs crates/engine/src/queue.rs crates/engine/src/title_task.rs crates/engine/src/subagents.rs crates/engine/tests`.
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED — timeout behavior changes every provider request; cancellation and terminal-event semantics must remain distinct.
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `74d34da`, 2026-09-21

## Why this matters

The engine currently accepts a `pi-core-rs` event stream and waits for its next
event without an engine-side idle deadline. A provider connection that stops
emitting SSE events can therefore leave a main Turn or child agent in
`streaming` forever; the observed failure left the parent Turn blocked until an
application restart. The fix must turn both "no first event" and "no next event"
into a normal provider failure, while preserving the existing user-cancelled
`aborted/interrupted` path.

The default should be 300 seconds, matching the documented default used by
Codex and the observed default in OpenCode. This plan deliberately does not add
automatic retries, a subagent wall-clock budget, a new RPC setting, or a
transcript schema change.

## Current state

The executor must preserve Holt's layering: the UI renders `MessagePart`s and
does not know provider events; `crates/engine` adapts `pi-core-rs`; no public
RPC type or serialized representation is needed for this fix. These constraints
come from `AGENTS.md:23-40` and `ARCHITECTURE.md:13-18,25-30`.

Relevant code at the planned commit:

- `crates/engine/src/agent.rs:543-546` stores the optional
  `EngineConfig::stream_fn` test seam on `AgentRuntime`.
- `crates/engine/src/agent.rs:1135-1143` defines `default_stream_fn`; it only
  calls `compat::stream_simple` and returns the raw
  `AssistantMessageEventStream`.
- `crates/engine/src/agent.rs:1668-1675` selects the injected/default stream
  and wraps it only with a pre-call cancellation check. It does not apply an
  idle deadline.
- `crates/engine/src/queue.rs:623-627` independently selects the runtime or
  default stream for manual compaction, so a fix only inside the main Turn
  function would miss this path.
- `crates/engine/src/title_task.rs:140-162` independently selects a stream.
  Title requests already have `TITLE_REQUEST_TIMEOUT`, but the stream helper
  must not make `stream.result().await` hang after a non-terminal upstream end.
- `crates/engine/src/subagents.rs:408-438` meters the parent-provided stream
  and passes it into a child `AgentRun`; the child must inherit the guarded
  stream rather than bypassing it.
- `crates/engine/tests/common/mod.rs:251-318` is the existing injected
  `ScriptedProvider` seam. `crates/engine/tests/scripted_provider.rs:12-101`
  shows the integration-test style and the existing expectation that terminal
  provider errors settle the session to `errored`.

The upstream stream type is `pi_core::agent::types::AssistantMessageEventStream`,
an event stream with `next()`, `result()`, `push()`, and `end()` operations. The
watchdog may wrap that type in Holt; do not edit the git checkout of
`pi-core-rs`, and do not change its dependency revision as part of this plan.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Drift check | `git diff --stat 74d34da..HEAD -- crates/engine/src/agent.rs crates/engine/src/queue.rs crates/engine/src/title_task.rs crates/engine/src/subagents.rs crates/engine/tests` | Empty output, or the listed drift is reviewed before proceeding |
| Format check | `cargo fmt -p holt-engine -- --check` | Exit 0 |
| Engine tests | `cargo test -p holt-engine` | Exit 0; all tests pass |
| Engine lint | `cargo clippy -p holt-engine --all-targets -- -D warnings` | Exit 0, or only the repository's already-documented dependency warnings if the command is run without `-D warnings` |
| Workspace check | `cargo check --workspace` | Exit 0 |
| Workspace tests | `cargo test --workspace` | Exit 0 |

`cargo fmt --all` is unreliable in this repository because the excluded
vendored GPUI tree carries a second workspace root; use the package-scoped
format command above, as documented in the existing plans index.

## Scope

**In scope** (the only files the executor should modify):

- `crates/engine/src/agent.rs` — watchdog implementation, stream selection
  wiring, diagnostics, and focused unit tests if that is the cleanest place for
  them.
- `crates/engine/tests/common/mod.rs` — only if a reusable never-ending or
  delayed scripted stream is required by an integration test.
- `crates/engine/tests/stream_timeout.rs` — optional new integration-test file
  if the existing test module is not a suitable home; do not create both test
  locations for the same cases.
- `plans/README.md` — executor status update after completion.

**Out of scope** (do not touch):

- `vendor/gpui/**` and the checked-out `pi-core-rs` dependency.
- `crates/rpc/**`, `crates/proto/**`, `crates/doc/**`, and all UI files.
- Provider settings, persisted configuration, or a new user-facing timeout
  control. Use an internal 300-second default in this plan.
- Automatic retries or retry backoff.
- A total Turn/subagent wall-clock deadline.
- Transcript persistence redesign or deduplication of streaming snapshots.

## Git workflow

- Match the repository's existing branch and commit conventions; if creating a
  branch, use `codex/009-stream-idle-timeout`.
- Commit message style is conventional and crate-scoped, for example:
  `fix(engine): fail stalled provider streams`.
- Do not push, merge, or open a PR unless the operator explicitly requests it.

## Steps

### Step 1: Add a reusable event-stream watchdog

In `crates/engine/src/agent.rs`, add a private helper that accepts a raw
`pi_core::agent::types::StreamFn` and an idle `Duration`, returning another
`StreamFn`. Keep the production constant at `Duration::from_secs(300)` and make
the duration an argument to the helper so unit tests can use milliseconds.

The helper must:

1. Clone the supplied `SimpleStreamOptions` (or construct defaults when it is
   `None`) and install a child `CancellationToken` in
   `options.base.base.signal`. The parent signal, when present, must remain the
   authority for user cancellation; cancelling the child on timeout must not
   mutate the parent token.
2. Call the raw stream function once and return its errors unchanged. After it
   returns, spawn a forwarding task that reads the upstream stream.
3. Wrap each `upstream.next()` in `tokio::time::timeout(idle, ...)`. Reset the
   deadline after every forwarded event, so the rule is idle time between
   events, not total request duration. The first `next()` is also covered.
4. Forward normal and terminal events unchanged. Track the latest non-terminal
   `partial` assistant message so a synthetic failure can preserve already
   received text/tool-call state.
5. On idle timeout, cancel the child token, log one structured warning with
   provider id, model id, timeout milliseconds, and phase (`first_event` or
   `between_events`), then push one terminal
   `AssistantMessageEvent::Error { reason: ErrorReason::Error, ... }` onto the
   output stream. The synthetic message must set `StopReason::Error` and a
   stable human-readable error message containing the timeout duration. Use the
   last partial message when available; otherwise initialize the message from
   the requested model and the assistant-message defaults.
6. If the parent cancellation token fires, cancel the child and resolve the
   output with an `ErrorReason::Aborted` terminal event (or forward the
   provider's terminal abort if it wins the race). This guarantees that a
   later `result().await` cannot hang, while the existing Turn cancellation
   logic still classifies the operation as interrupted.
7. If the upstream stream ends without a terminal event, resolve it as a
   provider error rather than calling `end(None)` and leaving `result()`
   pending forever.

Do not use a detached task that can continue consuming the provider forever
after timeout: cancelling the child signal and dropping the upstream stream
must happen on every terminal branch.

**Verify**: `cargo test -p holt-engine agent::` (or the narrowest available
agent unit-test filter) exits 0. If no existing filter reaches the new unit
tests, run `cargo test -p holt-engine` and confirm the full crate passes.

### Step 2: Route every engine-owned stream through the watchdog

Use the helper from Step 1 at the stream assembly boundary, without changing
the public `EngineConfig` type or the `StreamFn` signature.

- Make `default_stream_fn` return the guarded `compat::stream_simple` function.
- Guard an injected `EngineConfig::stream_fn` when it is stored by
  `AgentRuntime::new`, so tests and production use the same behavior.
- Preserve the existing cancellation pre-check in
  `run_agent_command_inner`; it is separate from the idle watchdog.
- Verify the runtime-selected stream used by `queue.rs` manual compaction and
  the default/injected stream used by `title_task.rs` are guarded through the
  same paths.
- Ensure the child path in `subagents.rs` receives the already-guarded parent
  stream. Do not add a second watchdog around the metered stream, which would
  create duplicate timers and duplicate diagnostics.

If the current ownership makes it impossible to guarantee exactly one wrapper
for both injected and default streams, stop and report the call sites that
would require a broader refactor; do not alter `pi-core-rs` to work around it.

**Verify**: `cargo test -p holt-engine --test scripted_provider` exits 0 and
the existing scripted text, tool-call, aborted, and provider-error tests retain
their current behavior.

### Step 3: Add focused timeout and cancellation tests

Add tests beside the watchdog helper or in one new
`crates/engine/tests/stream_timeout.rs`, following the existing tokio test and
`ScriptedProvider` conventions. Use a short injected timeout through the helper
argument; never wait five minutes in a test.

Required cases:

- **No first event**: a raw stream is returned but never pushes an event; the
  guarded stream emits one terminal error within the short timeout, and
  `result().await` resolves with `StopReason::Error` and the timeout message.
- **Inter-event stall**: forward one non-terminal event, then stop producing;
  the partial message is retained and the terminal error arrives after the
  idle interval.
- **Timer reset**: produce multiple events with gaps shorter than the timeout;
  assert no premature error, then stop and assert the eventual timeout.
- **User cancellation**: cancel the parent token before the idle interval;
  assert the result is aborted and the timeout diagnostic is absent.
- **Normal completion**: a terminal `Done` event passes through unchanged and
  does not create a warning or synthetic error.

If an end-to-end RPC test is added, assert the session reaches `errored` and
the transcript no longer contains an entry with `MessageStatus::Streaming`;
model the assertions on `crates/engine/tests/scripted_provider.rs:73-100` and
the helpers in `crates/engine/tests/common/mod.rs`.

**Verify**: `cargo test -p holt-engine --test stream_timeout` (if the new file
exists) and `cargo test -p holt-engine` both exit 0.

### Step 4: Run the repository verification gates

Run the commands in the table in this order: package format check, focused
tests, engine clippy, workspace check, and workspace tests. Inspect the diff
for scope and confirm that no dependency checkout, RPC type, transcript
schema, or UI file changed.

**Verify**: `git diff --check` exits 0; all commands pass; `git status --short`
lists only the in-scope implementation/test files and the plan-index status
update.

## Test plan

- Unit-level watchdog tests cover no-first-event, inter-event idle, timer reset,
  parent cancellation, normal terminal completion, and synthetic error
  resolution.
- Existing integration tests in `crates/engine/tests/scripted_provider.rs`
  remain the regression check for normal streaming and error settlement.
- If an integration fixture is needed, extend `ScriptedProvider` with a
  cancellation-aware delayed stream rather than adding a second provider
  abstraction.
- Verification: `cargo test -p holt-engine`, then `cargo test --workspace`.

## Done criteria

- [ ] A provider stream with no first event cannot leave a Turn waiting longer
  than the configured 300-second idle deadline.
- [ ] A stream that stops between events reaches a terminal provider error and
  cancels the underlying child signal.
- [ ] Parent cancellation remains an interrupted/aborted operation, not a
  timeout failure.
- [ ] Main Turns, subagents, compaction, title requests, and injected test
  streams all use one guarded stream path.
- [ ] `cargo fmt -p holt-engine -- --check` exits 0.
- [ ] `cargo test -p holt-engine` exits 0.
- [ ] `cargo clippy -p holt-engine --all-targets -- -D warnings` exits 0, or
  any baseline warning is explicitly reported.
- [ ] `cargo check --workspace` and `cargo test --workspace` exit 0.
- [ ] `git diff --check` exits 0 and no out-of-scope file is modified.
- [ ] The executor updates the Plan 009 row in `plans/README.md`.

## STOP conditions

Stop and report back instead of improvising if:

- The current stream API does not allow constructing a terminal event stream
  without modifying the external `pi-core-rs` checkout.
- A provider or test stream cannot observe the child cancellation token and
  would continue running after the watchdog drops it; document the leak before
  proceeding.
- The existing Turn finalization classifies the synthetic timeout as user
  cancellation, or a parent cancellation is rendered as a provider error.
- The current stream selection differs from the excerpts enough that wrapping
  `AgentRuntime::new` and `default_stream_fn` would double-wrap or bypass a
  request path.
- Any verification command fails twice after a reasonable targeted fix.
- The implementation requires changing an out-of-scope file, a public RPC
  method/type, serialized data, or the `pi-core-rs` dependency revision.

## Maintenance notes

The 300-second value is intentionally an internal default for this first fix.
If provider-specific values or a Settings control are added later, keep the
watchdog implementation independent from RPC serialization and validate that a
zero/disabled value has an explicit meaning. Any future retry policy must be
reviewed separately for duplicate billing and provider request idempotency.

Reviewers should inspect the race between timeout, parent cancellation, and a
simultaneous terminal event; verify that every `result().await` has a terminal
result; and confirm that a timed-out child cannot keep appending transcript
events after its parent Turn has settled. A separate future plan may add a
total subagent deadline, but it should not be conflated with this per-stream
idle watchdog.
