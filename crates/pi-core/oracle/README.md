# TypeScript oracle integrity

`pi-core/` (the TypeScript sources this crate ports) is git-ignored, so its
integrity is pinned by a committed manifest instead of the sources
themselves:

- `pi-core-manifest.txt` — the SHA-256 of every oracle file, prefixed with
  the upstream package versions (`@earendil-works/pi-telemetry`,
  `@earendil-works/pi-ai`, and `@earendil-works/pi-agent-core` **v0.84.4**).
  `package-lock.json` is excluded (dependency pinning, not oracle behavior)
  as are `node_modules/` artifacts.

- `scripts/generate-oracle-manifest.sh` — regenerates the manifest after
  restoring a fresh upstream tree. Never regenerate it to silence a
  verification failure unless the tree was deliberately updated.

- `scripts/verify-oracle.sh` — recomputes the tree digest and diffs it
  against the manifest; any local modification, addition, or removal of an
  oracle file fails the check. Run it before trusting oracle-generated
  fixtures (`scripts/oracle/*.mts`) or golden files.

To restore the oracle, obtain upstream v0.84.4 (the release tags of the
`earendil-works/pi` monorepo or the published npm tarballs for the three
packages), unpack it as `pi-core/`, and run
`scripts/verify-oracle.sh` — the manifest must match without regeneration.
