# A session worktree is chat-owned isolation, materialized at admission

The composer's "New worktree" checkout kind shipped as a draft-only
affordance long before the engine served it: the UI attached a
`WorktreeSpec { repoPath, base }` to the queued Run command, and the engine
ignored the field — every "New worktree" session silently ran in the main
checkout (the 2026-08-19 regression class). This ADR fixes the semantics:
**the isolation intent belongs to the chat, persists from command acceptance
onward, and resolves the run's working directory at admission.**

## Decisions

- **The intent is chat state, not per-request state.** `Chat.worktree` (an
  additive, serde-defaulted `WorktreeSpec` copy) is absorbed at command
  acceptance — `enqueue_run`, before the run becomes durable queue work — so
  a crash between the ledger write and admission cannot drop it (recovery
  clears `started` without re-reading the spec). Merge rules: unset fills,
  equal is idempotent, different is rejected. A chat whose `createChat`
  mutate never arrived gets its registry row minted at absorption. Later
  sends carry no spec of their own; admission resolves through the row, so a
  message queued behind the creating one — or sent after a failure — can
  never fall back to the main checkout.
- **Materialization happens at drain time**, after the queue's
  pending-to-started checkpoint and before skill resolution: resolve the
  effective cwd (`<data_dir>/worktrees/<chatId>`), re-check cancellation,
  then resolve `$` mentions and cut the Turn baseline against that directory
  — project skills come from the worktree. Creation is idempotent: an
  existing, correctly-registered worktree is reused as-is (dirty files,
  extra commits, and a user's branch switch are never undone, and the
  creation branch may be long gone); `base` applies only when the
  `holt/<chatId>` branch must be created. Registration damage is a loud
  per-message failure, never an automatic delete.
- **The worktree joins as its own Space** (CONTEXT.md: a worktree joins
  holt by being imported as its own Space — this is the auto-import). After
  materialization the engine ensures a `wt-<chatId>` Space and re-parents
  the chat: `cwd`, `space_id`, `checkout_id`, `branch`, and
  `source_context` all restamp in one admission. `checkout_id` matters
  most — Changes matches diffs by it first, so a stale id would keep
  matching the main checkout. Space first, chat re-parent second: a crash
  leaves an unclaimed Space row, which the next admission reuses. The UI
  follows the re-parent on the chats frame — the file tree roots by the
  selected chat's `space_id`, and `apply_chats` moves the project pick only
  when the selected chat's space actually changed.
- **A preparation failure is a visible, transcript-resident failure.** The
  user entry lands as admission would have written it, plus one system
  `Error` entry carrying the reason; the queue settles failed. No Turn ran,
  so there is no Turn frame and no terminal event (ADR-0019 stays
  execution-scoped), and the user entry has no History counterpart —
  `EditLastMessage` treats such entries as transcript-only edits.

## Consequences

- `CreateWorktree` / `DeleteWorktree` RPCs remain unserved (`UnknownMethod`):
  worktree lifecycle beyond creation-on-demand (listing, deletion, GC) is a
  later slice. A deleted chat leaves its worktree and Space behind until
  then.
- Subagent runs are unaffected: isolation is a main-chat admission property
  (ADR-0016 still denies subagents automatic worktrees).
- Legacy `WorktreeSpec`-carrying runs from older UIs degrade to the intent
  model on first absorption — the spec's `repoPath`/`base` become the chat's
  binding, and later spec-less sends resolve identically.
