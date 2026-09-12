# Plan 007: Isolate approval state across chats under concurrent tool execution

> **Executor instructions**: Follow this plan step by step. Run every verification command. Stop on any STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `163f10b`, 2026-09-12

## Why this matters

An `alwaysAllow` decision belongs to one chat, but the regression test becomes unable to create chat-2's approval gate when chat-1's bash preparation is delayed by real elapsed time. Approval state must remain independent across chats under concurrency.

## Current state

- `crates/engine/src/agent.rs:91-94` stores each chat's `GateGrants` in an `Arc<Mutex<_>>`; `:407-411` stores one engine-wide `ApprovalRegistry`.
- `crates/engine/src/gate.rs:292-466` checks grants, inserts pending approvals, waits on per-call oneshots, and records `AlwaysAllow` into the chat grant set.
- `crates/engine/src/rpc.rs:925-950` resolves approvals through the engine-wide registry by approval id.
- `crates/engine/tests/always_allow_rpc.rs:190-229` asserts chat-2 still reaches `pending` after chat-1 receives `alwaysAllow`.
- A 500ms bash `prepare` delay reproduces a timeout before chat-2's gate is created; `yield_now()` does not. The root cause must be traced rather than assumed.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Regression | `rtk cargo test -p holt-engine --test always_allow_rpc grants_are_scoped_to_their_chat -- --nocapture` | Passes repeatedly with delayed preparation |
| Full engine tests | `rtk cargo test -p holt-engine` | Only documented pre-existing git-status failures remain |
| Format | `rtk cargo fmt --all -- --check` | Exit 0 |
| Clippy | `rtk cargo clippy -p holt-engine --all-targets` | Exit 0 |

## Scope

**In scope:** `crates/engine/src/gate.rs`, `crates/engine/src/agent.rs`, `crates/engine/src/rpc.rs`, `crates/engine/tests/always_allow_rpc.rs`, and narrowly necessary test helpers.

**Out of scope:** shell PATH/shell selection, `pi-core-rs`, unrelated git-status failures, UI redesign, grant persistence, and policy changes.

## Steps

### Step 1: Build a deterministic concurrent reproducer and trace ownership

Add test-only delay/instrumentation at the bash preparation seam, or inject an equivalent fixture hook without changing production behavior. Run chat-1 and chat-2 concurrently and record chat id, tool-call id, grant check, approval insertion, transcript stamp, and RPC resolution. Identify whether the cause is shared registry state, queue/turn serialization, runtime-map collision, or test/provider sequencing.

**Verify:** the delayed test reproduces before the fix and logs distinct chat/tool ids at each transition.

### Step 2: Fix the smallest state/lifecycle boundary identified in Step 1

Keep `GateGrants` owned by `ChatRuntime`. Keep approval ids globally unique and RPC-resolvable, but ensure one chat's before-tool hook cannot suppress another chat's progress. If the cause is queue serialization, preserve per-chat ordering while allowing independent chats to progress; if registry/transcript routing is wrong, key the affected state by chat id or attach and validate chat ownership. Do not broaden grant matching or make grants engine-global.

**Verify:** the delayed regression passes 5 consecutive runs; chat-1's grant exempts only chat-1 and chat-2 reaches `pending`.

### Step 3: Remove temporary instrumentation and strengthen regression coverage

Retain a deterministic concurrent delayed-preparation test. Assert distinct pending approval ids, that resolving chat-1 cannot resolve chat-2, and that chat-2's later grant does not alter chat-1.

**Verify:** `rtk cargo test -p holt-engine --test always_allow_rpc` passes.

## Test plan

Extend `grants_are_scoped_to_their_chat` or add a neighboring test using the existing RPC polling helpers. Cover independent pending approvals, cross-resolution rejection/no-op, and preservation of per-chat grants.

## Done criteria

- [ ] Delayed concurrent regression passes 5 consecutive runs.
- [ ] Full `always_allow_rpc` integration test passes.
- [ ] Grants remain chat-owned; approval resolution preserves chat/call boundaries.
- [ ] Format and engine clippy pass.
- [ ] Only in-scope files are modified.

## STOP conditions

- The failure cannot be reproduced or is shown to be only a broken test helper.
- Fix requires changing `pi-core-rs` or public RPC payloads.
- Any proposed fix makes grants engine-global or permits unintended cross-chat approval resolution.

## Maintenance notes

Review every lock/await boundary in the hook and queue path. Future changes to per-chat scheduling, subagent grant inheritance, or registry cleanup must preserve independent progress and chat ownership. Keep shell environment work separate.
