# Turn diff baseline is a net-change filter

The "Latest turn" diff shows the **net working-tree changes since the
chat's last turn started**. The engine snapshots a baseline when a queued
command's run begins — `(HEAD sha at turn start, uncommitted patch at turn
start)` — and the turn diff is the `HEAD@start → workdir` diff filtered to
files whose uncommitted state differs from that patch.

## Considered options

- **HEAD-tree baseline only** (store just the sha): rejected because turns
  usually start on dirty trees, and every pre-existing uncommitted change
  would be misattributed to the turn — the primary use case fails.
- **Full workdir snapshot** (materialize a stash-create-like tree): exact
  semantics, but it must juggle `.git/index` or walk the full tree content
  once per turn, in a window where agent tools are concurrently writing
  files. Rejected as risky and expensive.

## Consequences

- On files already dirty at turn start the filter is file-accurate, not
  hunk-accurate: an edit that returns a file to its turn-start state drops
  out (net zero), which is the intended semantic.
- Baselines are in-memory and latest-per-chat; after an engine restart the
  turn scope reports a clear error rather than degrading to a working-tree
  diff.
