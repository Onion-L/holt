# Workspace git and Turn change sets

The git capability serving Space folders on the git2 backend, and the per-Turn net change sets derived from it: live diff watching, four diff modes, durable per-Turn records, and restore.

Not here: the agent's git surface is read-only — `write`/`edit`/`bash` never run git commands, and no staging/commit RPC exists (the UI-only write trio was removed; the Git panel hosts the commit graph only). The file sidebar's tree and tabs belong to workspace-files, though its status feed is computed here.

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| git2 access | The only module in the workspace that touches git2. Every git2 call runs on the blocking pool, never the async runtime; a per-checkout lock keyed by the repository's common git dir serializes same-repo operations while different repos proceed in parallel | [`crates/engine/src/git.rs`](../../crates/engine/src/git.rs) |
| Branch surface | `ListRefs` local branches default-first with `current` and linked-worktree paths; `SwitchRef` safe checkout (no force, no merge, no stash); `CreateBranch` `checkout -b` semantics that roll the fresh branch back when the checkout refuses | `git.rs` (`refs_snapshot`, `switch_branch`, `create_and_switch`) |
| Diff family | `GetCheckoutDiff`: `workingTree` (HEAD → index → workdir), `branch` (merge-base(baseRef, HEAD) → workdir: "what would this branch ship"), `commit` (parent → commit, root commits against the empty tree, never reading the live tree), `turn` (admission baseline → live or final tree) | `git.rs` (`capture_diff`, `commit_capture`, `turn_capture`) |
| Identity and checksums | Canonical checkout identity `sha256(deviceId ‖ NUL ‖ git_dir)`; diff checksum `sha256(head_sha ‖ NUL ‖ mode ‖ baseRef ‖ patch_bytes)` — the UI's checksum-keyed fold key | `git.rs` (`checkout_identity`) |
| Live watch | `WatchCheckoutDiffs`: one shared watcher per checkout, notify rooted at the space root, 200 ms quiet debounce; a failed attach keeps a 2 s polling fallback | [`crates/engine/src/git_watch.rs`](../../crates/engine/src/git_watch.rs) |
| Status feed | `WatchWorkspaceGitStatus`: one `statuses()` pass, untracked and ignored directories reported whole; a non-Git folder is no decorations and no error | [`git_status_watch.rs`](../../crates/engine/src/git_status_watch.rs), wired in `rpc/workspace.rs` |
| Turn change sets | The in-memory handoff: the chat's current Turn plus its most recently settled one, replaced at each admission; `Attribution` keeps only the Turn's own write paths | [`crates/engine/src/turn_changes.rs`](../../crates/engine/src/turn_changes.rs) |
| Durable records | One JSON per settled Turn under `turn-changes/{chatId}/{messageId}.json`: frozen summary plus immutable per-file before/after content; written once at settle, read lazily per request | [`crates/engine/src/turn_change_store.rs`](../../crates/engine/src/turn_change_store.rs) |
| Restore | Plain file I/O from a record — no git — gated per file on the disk still hashing to the record's `newContentHash` | [`crates/engine/src/turn_restore.rs`](../../crates/engine/src/turn_restore.rs) |
| Step checkpoints | Each tool call that changed files appends one line to `checkpoints/{chatId}/steps.jsonl` — Turn message id, tool call id and name, and every changed path's before/after side — with the exact bytes as blobs in the chat's bare store `checkpoints/{chatId}/objects` (ADR-step-checkpoints-in-a-chat-owned-object-store). Rides the attribution recorder, so a non-Git tree records nothing. A running call's start waits in `checkpoints/{chatId}/running/{toolCallId}.json` and goes when the call ends; one left by a crash is measured at the chat's next load into an interrupted step, whose changed files the History repair adds to that call's result (see persistence). `RewindToStep` undoes the steps after a point (see conversation-policy) and drops them from the log | [`crates/engine/src/checkpoints.rs`](../../crates/engine/src/checkpoints.rs), [`git/checkpoints.rs`](../../crates/engine/src/git/checkpoints.rs), wrapped around the tools in [`tools.rs`](../../crates/engine/src/tools.rs) (`ChangeAttribution`) |
| Session worktrees | A "New worktree" chat's isolation intent persists on the chat row at command acceptance (unset fills, equal is idempotent, different is rejected); at admission the intent — not the request's cwd — materializes `<data dir>/worktrees/<chatId>` on `holt/<chatId>` and registers Space `wt-<chatId>` before the chat re-parents | [`rpc/run.rs`](../../crates/engine/src/rpc/run.rs) (`absorb_worktree_intent`, `ensure_worktree_space`), `git.rs` (`materialize_worktree`) |
| RPC wiring | `ListRefs`/`ListBranches`/`SwitchRef`/`CreateBranch`, `GetCheckoutDiff`/`GetCheckoutFileDiffText`, `GetTurnChangeSet`/`WatchTurnChangeSet`/`RestoreTurnChanges`, `ListGitHistory`, `FetchAll` | [`crates/engine/src/rpc/git_surface.rs`](../../crates/engine/src/rpc/git_surface.rs) |
| UI cutaways | The Git panel (commit graph); the transcript's per-Turn change cards; one review companion surface (`RightSurface::TurnReview`) that re-aims instead of stacking | [`git_panel.rs`](../../crates/ui/src/git_panel.rs), [`changes/`](../../crates/ui/src/changes), [`turn_review.rs`](../../crates/ui/src/turn_review.rs) |

## Runtime path

1. **Watch.** The first `WatchCheckoutDiffs` subscriber roots a notify watcher at the space root (covering `.git/HEAD`); the last subscriber stops it. Arming blocks (the macOS backend's runloop can take seconds), so it runs on the blocking pool while the loop polls at 2 s until the attach lands. A frame fires only after 200 ms of quiet and only when that checkout's checksum actually changed.
2. **Read.** The diff modes share one capture pipeline under the checkout lock; `GetCheckoutFileDiffText` serves one file's full old/new sides (old from the mode's base tree, new from the working-tree file), with `stale` set when the checkout's checksum has moved past the pinned checksum the caller rendered. Patch text caps at 3 MiB, per-file sides at 1 MiB.
3. **Admission.** A Turn's start refreshes its baseline (the `turn` mode's base) and opens the attribution set: `write`/`edit` targets plus each bash command's before/after worktree delta. Subagent edits need no special handling — a child shares the parent's working directory and records into the parent's set.
   A session-worktree chat resolves its working directory first: the base ref applies only when `holt/<chatId>` must be created, an existing correctly-registered worktree is reused as-is, and a stale registration whose directory is gone is pruned and re-created.
4. **Settle.** Summary and per-file content are captured together in one locked pass and frozen; the record is written once. The in-memory handoff keeps the current and last-settled records; an engine restart drops them, and reads fall back to the persisted records — which outlive restarts, later workspace edits, and a working tree that can no longer be captured. Only a Turn with no record anywhere answers by its working directory.
5. **Review and restore.** Review fetches per-file unified diffs in `turn` mode addressed by `messageId` (a settled Turn serves its immutable stored pair; a live one reads the working tree). Restore confirms, then writes each file's stored old content back (or deletes an added file) — refused outright while the chat runs a Turn.

## Constraints, state, and failure behavior

- **Safe checkouts only.** A switch that would overwrite uncommitted data fails with git's own message and leaves the working tree and HEAD untouched; a refused `CreateBranch` rolls the branch back so a retry is not met with "already exists".
- **Fetch never prompts.** System-default credentials (helpers / ssh-agent), one attempt — a credential retry loop must never become an interactive prompt — wrapped in a 30 s timeout, prune on.
- **Attribution is a fence.** A change set keeps only files in the Turn's own write set, so another chat's concurrent work in the same tree never lands in this Turn's card.
- **Records are immutable.** Only a replayed settle of the same Turn atomically replaces its own file. A malformed record degrades to "no record" with a warning — a damaged history never blocks the chat. Deleting a chat deletes its whole `turn-changes/{chatId}` directory.
- **Restore refuses, never merges.** Per file: truncated, binary, lossy (stored text must hash to the original bytes), and unsafe paths (`.git`, symlinks, escapes) are refused; a file that moved on since settlement is a `Conflict`, reported as `LaterTurn` when a later settled Turn shares the path — with the overlapping Turns named in the reply. A repeated restore is `AlreadyRestored`. Writes go through a sibling temp + rename, keeping the file's permissions.
- **A session worktree never falls back.** A failed materialization or Space conflict persists the user entry plus a system Error entry and runs no Turn; later sends still resolve through the intent. A foreign directory or mis-registered worktree at the target path is reported, never deleted.
- **A step's before side is exact bytes, never rebuilt from patch text.** write/edit read their target before the call; a bash window stores every uncommitted file's bytes before the command (identical content stored once) and copies clean files' HEAD blobs after it. Files over 10 MiB and non-regular files keep only their path (`omitted`); a call that changed nothing, or whose window could not store its before sides, records no step. Every checkpoint write is best-effort — it never fails the tool. Deleting a chat deletes `checkpoints/{chatId}`.
- **Non-Git is typed, not an error.** A non-Git working directory answers `Unsupported` — never an empty change set, never a capture error; "no Turn recorded" only when no record exists anywhere.

## Verification

```sh
cargo test -p holt-engine git
cargo test -p holt-engine turn_change
cargo test -p holt-engine turn_restore
cargo test -p holt-ui turn_review
cargo test -p holt-engine --test all worktree
```

The engine suites cover the deterministic checksum, safe-checkout refusals, the four capture modes, debounce timing (injected clock, no sleeps), record write-once and malformed-degrade, and restore's full refusal taxonomy (conflict, overlap, rename, idempotence, unsafe paths). The `turn_change_set` integration suite also covers step checkpoints: exact bytes across write, edit and bash (CRLF and no final newline included), no step for a no-op call or a non-Git tree, removal with the chat, a bash call cut off mid-run reaching the resumed model with the files it changed, and `RewindToStep` (plan, apply, conversation cut, the all-or-nothing conflict refusal, and a crashed rewind finished on load). The UI suite covers the review pane. The `worktree` integration suite covers intent persistence across queued and spec-less sends, loud preparation failures, branch and worktree reuse, and path/Space collisions.
