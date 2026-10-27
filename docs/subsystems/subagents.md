# Subagents

Foreground delegation inside a parent Turn: the parent model calls the `Agent` tool, a child run starts with its own History and its own transcript document, and only its bounded final summary enters the parent conversation. Children are workers, not peers — they cannot delegate, and their whole run books to the parent.

Not here: the run loop a child executes (see run-loop — a child is one `run_agent_command` with a narrowed toolset); the permission gate (children inherit the parent Turn's mode, and child Approvals project into the parent transcript with a typed origin — see permission-gate); usage booking (see persistence — one `subagent` kind, stamped with the child doc id).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Roles | Two fixed roles inheriting the parent Turn's model, reasoning level, working directory, and permission mode. **Explorer** keeps the read-only set (`ls`, `read`, `grep`, `read_chat`, `web_fetch`, plus `web_search` only when the parent Turn resolved a backend); **Worker** adds `write`, `edit`, `bash`. Neither delegates | [`crates/engine/src/subagents.rs`](../../crates/engine/src/subagents.rs) (`system_prompt`) |
| Briefing | Each child starts with independent History, a task brief, applicable ancestor `AGENTS.md` instructions, and a fresh skills listing | `subagents.rs`, `agent.rs` |
| Concurrency | An engine-wide semaphore admits eight running children, shared across every chat; a parent Turn may create any number (waves of children — the orchestrator shape). No fixed request-count or total-runtime cap | `subagents.rs` (`MAX_RUNNING_CHILDREN`) |
| Cancellation | Descends from the parent Turn — Stop and Steer wait for child cleanup before the next Turn. A child failure returns an error and partial output without cancelling siblings | `subagents.rs` |
| Child documents | `{parentChatId}--sub--{uuid}` identities with independent transcript and History files under `subagents/<parentChatId>/`; they reuse the transcript runtime and event folding but never join the chat registry, session list, or queue consumer. The child's queue is unused and never persisted | `subagents.rs`, [`store.rs`](../../crates/engine/src/store.rs) |
| Result handoff | Only the final summary enters parent History, bounded to 12,000 tokens measured with the shared o200k tokenizer — a stable output budget, not a claim about any provider's billing tokenizer. The full final output persists under the child's `results/` directory; truncation carries an explicit notice and an absolute path usable by `read`. The independent transcript retains the intermediate work | `subagents.rs` (`RESULT_TOKENS`) |
| Blobs | `FetchToolBlob` serves finished child transcripts as JSON text for a `{parentChatId}/{subagentDocId}` reference; live documents stream through the ordinary transcript watch | [`rpc/`](../../crates/engine/src/rpc) |
| Recovery | Child records survive restart for viewing; running records recover as aborted with failed parent spawn chips — execution is never resumed. Deleting the parent deletes the child records | `subagents.rs` |
| UI cutaways | Spawn chips in the parent transcript (a random name + pastel avatar identity drawn without replacement, keyed by the call id pre-spawn and adopted onto the subagent doc id — the doc-keyed half persists to `agent-identities.json`, so a reopened chat keeps its faces; live tail, completion status); the child tab opened from a chip or a projected child Approval | [`crates/ui/src/transcript/`](../../crates/ui/src/transcript) |

## Runtime path

1. **Spawn.** The parent calls `Agent` with a role and a task; the harness admits the child through the semaphore, mints the doc id, and mounts the role's toolset.
2. **Run.** The child is one admitted run: same transport guard, same retry policy, its own cancellation descending from the parent. Its approvals surface in the parent transcript; always-allow grants are shared with the parent chat.
3. **Overflow.** A child compacts against its own History like any run; a context overflow that compaction cannot absorb fails the child — no retry, no model switch.
4. **Finish.** The summary (bounded) lands in the parent's History as the tool result; the full output goes to `results/`; the spawn chip settles.

## Constraints, state, and failure behavior

- **Foreground or nothing**: children are created and awaited inside the parent Turn; there is no background queue of agents, and a child never outlives its parent's Turn except as a viewable record.
- **The parent assigns file ownership** — workers share the parent's actual working directory; there are no automatic worktrees, merging, or rollback (the parent's change-set attribution covers the whole Turn; see workspace-git).
- **A child's failure is content, not a crash**: the parent model reads the error and partial output and can route around it.
- **Planning Turns mount no `Agent` tool** — delegation is a main-chat capability (see modes).

## Verification

```sh
cargo test -p holt-engine subagent
cargo test -p holt-engine --test all subagent
```

The suites cover role toolsets and briefing, the eight-slot concurrency cap and its Holt-wide sharing, cancellation descent and sibling isolation, doc-id minting and persistence layout, the bounded summary with its truncation notice and results file, recovery-as-aborted, and parent-side projection of child approvals.
