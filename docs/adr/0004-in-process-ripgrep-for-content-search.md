# In-process ripgrep crates for content search

The agent's content-search tool runs ripgrep's own crates in process
(`ignore` for walking/gitignore, `grep-regex` + `grep-searcher` for
matching, line numbers, binary detection, UTF-16 transcoding, and context
lines) instead of shelling out to a system `rg` binary. A system binary was
rejected for the same cause as system `git` in ADR-0001 — it requires the
user to have `rg` installed (macOS ships without it) and drifts across
versions — and hand-rolling the search on `regex` alone would re-implement
what `grep-searcher` already provides.

## Consequences

- Three more pure-Rust crates.io dependencies; `ignore` was already declared
  in the workspace and unused — this is the use it was staged for. `regex`
  is never a direct dependency; `grep-regex` encapsulates it.
- Search behavior upgrades only when the lockfile does; it does not track
  the user's installed ripgrep. That is the point.
