---
name: code-review
description: "Adversarial dual-axis code review skill: audit changes comprehensively across both the Problem Axis (did it genuinely solve the issue?) and the Implementation Axis (is the code sound, safe, and architecturally compliant?). Use when asked to review code, check a PR diff, or verify changes before merging."
---

# Code Review (Dual-Axis)

Adversarial inspection skill: audit code changes along two independent, mandatory
axes before any merge:
1. **Problem / Spec Axis**: Did the change genuinely solve the issue, address the
   root cause, satisfy every acceptance criterion, and avoid regressions?
2. **Implementation / Engineering Axis**: Is the code architecturally sound,
   concurrency-safe, robustly error-handled, thoroughly tested, and documented in
   living subsystem contracts?

A code review is not a rubber stamp or a debate about subjective style. It is a
rigorous gatekeeper. A PR that writes clean code but misses the bug's root cause fails;
a PR that fixes the bug via an architectural breach or a brittle hack also fails. Both
axes must pass.

## Review targets

The skill reviews either an existing GitHub PR or a local working branch:

- **Review a PR**:
  1. Fetch PR details and diff:
     `gh pr view <N> --comments`
     `gh pr diff <N>`
  2. Extract the linked issue number from the PR body (`Closes #<ID>` or `Relates to #<ID>`).
  3. Fetch the full issue spec:
     `gh issue view <ID>`

- **Review a local branch**:
  1. Diff against `main`:
     `git diff origin/main..HEAD`
     `git diff --stat origin/main..HEAD`
  2. Identify the target issue from the branch name or prompt, and fetch via `gh issue view <ID>`.

---

## Axis 1: Problem / Spec Verification (是否真正解决了 Issue)

Focus: **"Doing the right thing."** Evaluate whether the change genuinely resolves
the user's problem and satisfies the agreed spec without introducing collateral damage.

### 1. Root Cause vs Superficial Patch (根因 vs 掩盖症状)
- Does this change eliminate the underlying cause, or does it merely silence an error
  downstream (e.g. adding a blanket fallback, swallowing an exception, or returning dummy data)?
- If fixing a bug: was the defect reproduced with a failing test *before* the fix was applied?
- **Red flag**: Catch-all error ignoring, arbitrary retries, or modifying callers to compensate
  for a broken callee invariant.

### 2. Spec Acceptance Criteria (验收项逐一闭环)
- Read `## Acceptance Criteria` in the linked Issue Spec. With no linked
  issue, the spec is the PR body itself (ADR + spec-grade What/Why, Fence,
  and Verification — see the `pr` skill); review against those sections,
  and a body that cannot serve as the spec is itself the finding.
- Every single bullet point must have concrete verification evidence in the diff (an automated
  unit/integration test or an explicit verification command output).
- **Red flag**: Criteria claimed as "tested manually" without reproducible automated assertions.

### 3. Edge Cases & Regressions (边界条件与负向场景)
- Check boundary inputs: empty collections, null/zero values, huge payloads, Unicode/escapes.
- Check environment faults: disconnected network, unserved RPC endpoints (`UnknownMethod`),
  cancellation during in-flight operations, process interrupts.
- Are existing features or neighboring code paths broken or altered?
- **Red flag**: Happy-path-only testing; failure modes left to unwrap or panic.

### 4. Fence & Blast Radius (修改边界防守)
- Read `## Fence` in the Issue Spec — or the PR body's Fence section when
  the spec is in-PR: did the diff stay strictly within permitted
  crates/modules?
- Did the author sneak in unrelated refactors, format churn, or drive-by renames?
- **Red flag**: Diff touching shared core types or config files outside the agreed fence.

---

## Axis 2: Implementation & Engineering Audit (全方位代码实现审查)

Focus: **"Doing things right."** Evaluate whether the code meets Holt's rigorous
architectural invariants, safety constraints, and code standards.

### 1. Architecture & Hard Boundaries (架构四条硬边界)
Verify the four non-negotiable boundaries from [ARCHITECTURE.md](../../../ARCHITECTURE.md):
1. **UI boundary**: UI speaks strictly through `rpc::methods` — never direct calls to engine
   internals, disk, or memory state.
2. **Core adapter**: `engine` is `core`'s only adapter.
3. **Git & Filesystem encapsulation**: `git2` lives strictly in `engine::git`; workspace disk
   I/O lives strictly in `engine::files`.
4. **Disk write ownership**: All disk writes are engine-owned.
- **Red flag**: UI components importing engine private structs; direct git shelling or raw
  filesystem writes scattered across random modules.

### 2. Concurrency, Async & Locks (异步与并发安全)
- **Lock discipline**: Are synchronization locks held across an `.await` point? (Instant deadlock
  risk). Are lock scopes kept as narrow as possible?
- **Cancellation safety**: Can futures be aborted at any yield point without leaving state half-updated?
- **Channels & Queues**: Are bounded channels properly awaited, or can full channels deadlock the loop?
- **Red flag**: `block_on` inside async tasks; long-lived `std::sync::MutexGuard` spanning I/O.

### 3. Error Handling & Robustness (错误处理与健壮性)
- No `unwrap()` or `expect()` in production paths unless statically proven unreachable (with comment).
- Errors must use typed enums or project errors, preserving cause chains.
- State mutation must be atomic or transactional: if step 2 fails, does step 1 leave orphan state?
- **Red flag**: `.ok()` discarding errors silently; panicking in library code.

### 4. Test Quality & Rigor (测试质量与确定性)
- Are tests truly deterministic? (Injected clocks, no `sleep(100ms)`, no reliance on host timezone).
- Do test assertions actually verify behavior, or are they trivial asserts (e.g. `assert!(result.is_ok())`)?
- Were any existing tests deleted or weakened just to get CI green?
- **Red flag**: Flaky sleeps; testing mocked mocks instead of real subsystem interactions.

### 5. Living Subsystem Contracts & Standards (活体文档同步)
- Per [docs/subsystems/README.md](../../../docs/subsystems/README.md):
  - If runtime behavior, RPC contracts, state transitions, or interfaces changed, is the owning
    `docs/subsystems/*.md` updated **in the same commit**?
  - If a subsystem was deleted, was its page deleted?
  - Did a major architectural decision (rejected alternatives, cross-module impact) get appended
    to [docs/DECISION.md](../../../docs/DECISION.md) as `ADR-<slug>`?
- Verify against [docs/CODE-STANDARDS.md](../../../docs/CODE-STANDARDS.md)
  and comment integrity (no dead links, no speculative `TODO`s without an issue link).
- **Red flag**: Code behavior altered with zero documentation update in `docs/subsystems/`.

---

## Output Verdict Schema

Every review report must strictly follow this dual-axis structure:

```markdown
# Code Review: <PR #N or Branch Name>
**Linked Spec**: Issue #<ID> (<Issue Title>) — or "in-PR spec (ADR <slug>)"
**Verdict**: `APPROVE` | `REQUEST_CHANGES` | `BLOCKED`

---

## Axis 1: Problem / Spec Verification (是否解决 Issue)
| Inspection Item | Status | Evidence / Notes |
| :--- | :--- | :--- |
| **Root Cause vs Symptom** | Pass / Fail | <Addresses root cause or only symptoms?> |
| **Acceptance Criteria** | Pass / Fail | <Checklist of every criterion from Issue Spec> |
| **Edge Cases & Regressions** | Pass / Fail | <Empty inputs, errors, unserved RPCs handled?> |
| **Fence & Blast Radius** | Pass / Fail | <Changes strictly confined to allowed scope?> |

### Axis 1 Detailed Findings
- [x] Criterion 1: Verified by `<test_file::test_name>`
- [ ] Criterion 2: <Missing verification or unhandled edge case>

---

## Axis 2: Implementation & Engineering Audit (代码实现全方位审查)
| Inspection Item | Status | Evidence / Notes |
| :--- | :--- | :--- |
| **4 Hard Boundaries** | Pass / Fail | <Checked vs ARCHITECTURE.md boundaries> |
| **Async, Concurrency & Locks** | Pass / Fail | <No locks held across await; cancellation safe> |
| **Error Handling & Robustness** | Pass / Fail | <No unwrap/panics; clean error propagation> |
| **Test Quality & Determinism** | Pass / Fail | <Real assertions; no sleeps; deterministic> |
| **Living Docs (subsystems/)** | Pass / Fail / N/A | <Owning docs/subsystems/*.md updated in diff> |
| **Code Standards & Comments** | Pass / Fail | <Compliant with CODE-STANDARDS.md, no dead links> |

### Axis 2 Detailed Findings
- <Specific architectural, lock, or error-handling observations>

---

## Actionable Line Findings
1. **`<path/to/file.rs#L12-L34>`**:
   - **Axis**: `[Problem | Implementation]`
   - **Severity**: `[Blocker | Major | Nitpick]`
   - **Problem**: <concise explanation of failure, boundary breach, or missing contract>
   - **Remedy**: <exact suggested fix or test to write>

---

## Final Verdict & Next Steps
- **`APPROVE`**: Both axes pass with zero Blocker/Major findings. Ready to merge.
- **`REQUEST_CHANGES`**: Fails on either axis. State exact blockers to resolve.
- **`BLOCKED`**: The diff reveals that the model cannot complete this task safely (missing external environment, unsolvable spec contradiction, capability wall). Recommend labeling the issue `blocked:agent`.
```
