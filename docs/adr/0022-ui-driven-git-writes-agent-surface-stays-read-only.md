# UI-driven git writes enter the engine; the agent surface stays read-only

The Git panel (working-tree status, staging, commit) requires the engine's
first content-mutating git operations. We added exactly three RPC methods —
`StagePaths` / `UnstagePaths` / `CommitStaged` — implemented in
`crates/engine/src/git.rs` under the existing per-checkout lock. This is a
deliberate boundary: the writes exist for the UI only. `engine::tools` gains
no stage/commit capability, and the agent's standing prohibition on
committing (see `system_prompt.md`) is unchanged. Wiring these methods into
the agent tool surface is a future decision that must be made explicitly,
not by proximity.

Commit identity comes from the repo's effective git config
(`user.name` / `user.email`), author equals committer, exactly like
`git commit`; a missing identity fails with an actionable error. Holt does
not grow its own identity configuration.

`CommitStaged` refuses whenever `repo.state()` is not `Clean` (merge,
rebase, revert, cherry-pick in progress). git2's `commit()` does not pick up
`MERGE_HEAD` automatically, so committing with default parents mid-merge
would silently produce a commit that loses the merge parentage — wrong
history, not merely an "early merge conclude." For the same reason the
engine refuses to stage or unstage conflicted paths (`git add` on a
conflicted path marks it resolved). UI-level disabling of these actions is
presentation sugar; the gate lives in the engine.

The status payload grows rather than forks: `WorkspaceGitStatusEntry` gains
optional `index` / `worktree` porcelain kinds and an `is_dir` flag
(untracked directories arrive as single collapsed entries whose trailing
slash must survive to the UI), while the collapsed `kind` keeps its meaning
for the file sidebar — with one narrowing: it now reports `Conflicted`
honestly instead of folding conflicted paths into `Modified`, so both
consumers see conflict state without deriving it. The existing status watch is reused
as-is — it already watches `.git` precisely so that staging moves and HEAD
switches emit frames ("covering the working tree AND `.git` … which the
checkout-diff watch's checksum gate would miss"), so staging, external
terminal git operations, and branch switches all reach the panel with no new
machinery.

Path arguments must be repo-relative; absolute paths and `..` components are
rejected. Unstaging a path that is not in the index is a successful no-op,
matching `git reset -- <path>`.
