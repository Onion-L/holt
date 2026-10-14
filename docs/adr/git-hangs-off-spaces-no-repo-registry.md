# Git capability hangs off Spaces; no repo registry

The engine's git surface serves the folders of **spaces** — the existing
(device, folder) unit the UI organizes around. There is no separate repo
registry: `ListRepos` / `AddRepo` / `CloneRepo` / `CreateRepo` stay
unserved (dead contract constants, by design), and a checkout's canonical
identity is `sha256(deviceId ‖ NUL ‖ git_dir)`, minted by the engine for
git-detected spaces at create time and backfilled lazily for persisted
spaces.

A parallel repo registry would duplicate what spaces already model and
re-open identity questions (two spaces on one repo, future worktrees).
Diff resolution keys on checkout id first, falling back to device+cwd and
cwd only — the fallbacks exist for pre-backfill rows, not as the design
center.
