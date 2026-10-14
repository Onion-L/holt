# git2 as the engine's git backend

The engine's git slice (branches, switching, checkout diffs, history) is
implemented on `git2` (libgit2 bindings). It covers safe checkout,
merge-base, topo-sorted revwalk, and workdir diffing in one library; the
alternatives were rejected for cause — `gix` for API churn across a very
large surface, and shelling out to system `git` because it requires git
installed, invites porcelain-parsing fragility, and drifts across versions.

## Consequences

- The workspace gains its first C dependency (`libgit2-sys`, vendored
  source built by cargo) in a repo that otherwise freezes third-party
  assets. This is deliberate: it is a crates.io build, not another snapshot
  to maintain under `third_party/`.
- Checksums and patch shapes (ADR-turn-diff-net-change-baseline, `CheckoutDiff`) are produced by
  libgit2's diff printer; swapping the git backend later re-opens those
  semantics.
