# Holt Domain Glossary

- **Workspace registry**: typed index of devices, spaces, chats, and session status rows.
- **Watch**: a live engine stream that keeps one UI snapshot current.
- **Watch coordinator**: the internal module that owns watch subscription, frame decoding, retry, and cancellation policy.
- **Workspace registry adapter**: a storage-specific implementation of the workspace registry interface, such as Loro or HLC/overlay storage.
- **Space**: a synced (device, folder) pair — the unit of organization in the sidebar. A space's folder may or may not be a git work tree.
- **Checkout**: the canonical identity of one working copy of a git work tree, `sha256(deviceId ‖ NUL ‖ git_dir)`. Diffs are grouped per checkout, not per folder path.
- **Turn**: one agent run of a chat — starts when its queued command begins running, ends when the run finishes or is interrupted.
- **Diff scope**: which comparison a Changes pane shows. Four flavors: **working tree** (uncommitted changes vs HEAD), **branch** (everything the branch adds over the merge-base with a base ref, working tree included), **latest turn** (net changes since the chat's last turn started), **history** (the commit graph); a fifth, **commit**, exists only as a pinned per-commit pane.
- **Base ref**: the branch a branch diff is taken against; defaults to the repo's default branch.
- **Turn diff**: the net working-tree changes since the chat's last turn started. Pre-existing uncommitted changes are not part of a turn unless the turn touched them.
_Avoid_: agent diff, session diff
