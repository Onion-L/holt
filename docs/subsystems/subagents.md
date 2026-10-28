# Subagents

Foreground delegation inside a parent Turn: the parent model calls the `Agent` tool, a child run starts with its own History and its own transcript document, and only its bounded final summary enters the parent conversation. Children are workers, not peers — they cannot delegate, and their whole run books to the parent.

Not here: background orchestration of many children by a script (see workflows — its `agent()` calls run through this page's child path); the run loop a child executes (see run-loop — a child is one `run_agent_command` with a narrowed toolset); the permission gate (children inherit the parent Turn's mode, and child Approvals project into the parent transcript with a typed origin — see permission-gate); usage booking (see persistence — one `subagent` kind, stamped with the child doc id).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Roles | Two fixed roles inheriting the parent Turn's model, reasoning level, working directory, and permission mode. **Explorer** keeps the read-only set (`ls`, `read`, `grep`, `read_chat`, `web_fetch`, plus `web_search` only when the parent Turn resolved a backend); **Worker** adds `write`, `edit`, `bash`. Neither delegates | [`crates/engine/src/subagents.rs`](../../crates/engine/src/subagents.rs) (`system_prompt`) |
| Briefing | Each child starts with independent History, a task brief, applicable ancestor `AGENTS.md` instructions, and a fresh skills listing | `subagents.rs`, `agent.rs` |
| Concurrency | An engine-wide semaphore admits eight running children, shared across every chat; a parent Turn may create any number (waves of children — the orchestrator shape). No request-count, token, or total-runtime cap: spend is made visible (per-chip and per-group token totals) and stoppable, never bounded | `subagents.rs` (`MAX_RUNNING_CHILDREN`) |
| Fan-out | `Agent` with `items` runs one child per item from one call: `{{item}}`/`{{index}}` fill description and prompt (a prompt without `{{item}}` gets it appended). Each child gets a display-only spawn part `{callId}#item-{n}` inserted right after the call's part — queued (unresolved, no ref) until admitted, settled by the fan-out itself — and the UI draws those in place of the container. The result is a JSON Lines file (`results/fanout-<uuid>.jsonl`, item order: index, item, status, subagent, result or error); the call fails only when every child failed | `subagents.rs` (`execute_fanout`), [`transcript/model.rs`](../../crates/ui/src/transcript/model.rs) |
| Cancellation | Descends from the parent Turn — Stop and Steer wait for child cleanup before the next Turn. A child failure returns an error and partial output without cancelling siblings | `subagents.rs` |
| Child documents | `{parentChatId}--sub--{uuid}` identities with independent transcript and History files under `subagents/<parentChatId>/`; they reuse the transcript runtime and event folding but never join the chat registry, session list, or queue consumer. The child's queue is unused and never persisted | `subagents.rs`, [`store.rs`](../../crates/engine/src/store.rs) |
| Structured results | With `output_schema` (a JSON Schema of type object) the child mounts `submit_result`, whose parameters are that schema — core validates each call, so a bad submit returns an error the child retries. A valid call terminates the child's run and becomes its result, as JSON (`results/<id>.json`); ending without one fails the child | `subagents.rs` (`submit_result_tool`) |
| Result handoff | `result_mode: "file"` returns only status and the result file path (fan-out: the summary line). Otherwise only the final summary enters parent History, bounded to 12,000 tokens measured with the shared o200k tokenizer — a stable output budget, not a claim about any provider's billing tokenizer. The full final output persists under the child's `results/` directory; truncation carries an explicit notice and an absolute path usable by `read`. The independent transcript retains the intermediate work | `subagents.rs` (`RESULT_TOKENS`) |
| Blobs | `FetchToolBlob` serves finished child transcripts as JSON text for a `{parentChatId}/{subagentDocId}` reference; live documents stream through the ordinary transcript watch | [`rpc/`](../../crates/engine/src/rpc) |
| Recovery | Child records survive restart for viewing; running records recover as aborted with failed parent spawn chips — execution is never resumed. Deleting the parent deletes the child records | `subagents.rs` |
| UI cutaways | Spawn chips in the parent transcript (a random name + pastel avatar identity drawn without replacement, keyed by the call id pre-spawn and adopted onto the subagent doc id — the doc-keyed half persists to `agent-identities.json`, so a reopened chat keeps its faces; live tail, completion status); every agent-only group starts as one folded row — at most five faces (running first) and a lifecycle count with the settled children's token total ("30 subagents · 8 running · 12 queued · 2 failed · 8 done · 1.2M tokens") — expanding to its railed spawn chips; the child tab opened from a chip or a projected child Approval | [`crates/ui/src/transcript/`](../../crates/ui/src/transcript) |

## Runtime path

1. **Spawn.** The parent calls `Agent` with a role and a task (or a fan-out over `items`); the harness admits each child through the semaphore, mints the doc id, and mounts the role's toolset (plus `submit_result` under an `output_schema`).
2. **Run.** The child is one admitted run: same transport guard, same retry policy, its own cancellation descending from the parent. Its approvals surface in the parent transcript; always-allow grants are shared with the parent chat.
3. **Overflow.** A child compacts against its own History like any run; a context overflow that compaction cannot absorb fails the child — no retry, no model switch.
4. **Finish.** The summary — or submitted JSON — goes to `results/` and returns bounded, or as a path in file mode; a fan-out collects its children into one JSON Lines file. The spawn chip (or each fan-out part) settles.

## Constraints, state, and failure behavior

- **Foreground for `Agent`**: its children are created and awaited inside the parent Turn and never outlive it except as a viewable record. Background children exist only inside a workflow run (see workflows).
- **The parent assigns file ownership** — workers share the parent's actual working directory; there are no automatic worktrees, merging, or rollback (the parent's change-set attribution covers the whole Turn; see workspace-git).
- **A child's failure is content, not a crash**: the parent model reads the error and partial output and can route around it.
- **Planning Turns mount no `Agent` tool** — delegation is a main-chat capability (see modes).

## Verification

```sh
cargo test -p holt-engine subagent
cargo test -p holt-engine --test all subagent
```

The suites cover role toolsets and briefing, the eight-slot concurrency cap and its Holt-wide sharing, cancellation descent and sibling isolation, doc-id minting and persistence layout, the bounded summary with its truncation notice and results file, structured results (validation retry, missing submit), file mode, fan-out parts and JSON Lines, recovery-as-aborted, and parent-side projection of child approvals.
