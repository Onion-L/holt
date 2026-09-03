# A branch is the working directory's live HEAD, not chat state

A chat's branch used to be fixed at creation ("refs fixed at creation": the
picker refused to move an existing chat, and `createChat` stamped the branch
once and forever). We reversed that. **A branch switch is allowed at any
time, including while a Turn runs.** The session footer's branch chip is
live; picking a ref performs a safe `git switch` of the chat's working
directory immediately. A Turn is never interrupted — the next Turn runs on
whatever the working directory holds when it starts, and the engine stamps
`chat.branch` + `source_context` from HEAD at that moment (alongside the
existing per-Run `row.cwd` restamp; no `RunRequest` change, no new Mutate).
There is no pending or queued target: the local repo is the single source of
truth, and the chip renders the working directory's current branch.

## Considered options

- **Defer the switch while any Turn occupies the folder**: rejected — "a
  Turn is a Turn". Git's own safety is the only guard: the switch is always
  safe-checkout (never discards, merges, or stashes), and when git refuses
  because of conflicting uncommitted changes, a dialog informs and lists the
  blocking files. No force-switch escape hatch.
- **Per-chat pending intent (the pick lands at the next send)**: rejected —
  half-effective states ("picked but not yet running there") are what the
  label machinery cannot represent honestly, and they reintroduce a second
  code path. Switching immediately keeps one rule for drafts and sessions.
- **Mid-session worktree moves via `set_chat_cwd`** (documented in the proto
  crate for the real backend): rejected for this slice. A chat's working
  directory is fixed at creation and always its space's folder; only the
  branch inside it switches.

## Consequences

- A clean-tree switch under a live Turn changes files beneath the running
  agent, silently. Accepted: the agent re-reads files, the transcript stays
  complete, and guarding it would mean reintroducing the lock.
- A ref already checked out in another worktree can never be switched to —
  git refuses, and we do not redirect the chat into that worktree (the
  `ReuseWorktree` checkout plan is deleted everywhere, new chats included).
  Worktrees join holt as their own Spaces; picking their ref anywhere shows
  a dialog saying the branch is checked out in that worktree.
- A turn-diff baseline (ADR-0003) can span a mid-Turn branch switch; the
  turn diff then shows the branch delta as net changes. Accepted quirk.
- The conversation is never reset on a target change — moot now that cwd
  never moves, but recorded as a deliberate divergence from the proto
  crate's `set_chat_cwd` ("the next run in the new folder starts a fresh
  provider conversation by design"), which the real backend may layer on
  its own when it lands.
- Legacy chats whose cwd is a worktree (minted by the deleted ReuseWorktree
  plan) keep working; their branch switches happen inside their own folder.
