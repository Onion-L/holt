# The agent core is a Holt-owned workspace crate

## Context

The agent loop (`pi-core-rs`, a Rust port of pi v0.84.4: `pi-agent-core`,
`pi-ai`, `pi-telemetry`, ~63.5k lines) lived in a separate repository and
rode into Holt as a pinned git dependency. Every fix required a push to that
repository plus a `Cargo.lock` bump here, so a one-line core fix could never
land atomically with the engine change that needed it (a compaction panic
forced exactly that two-step). Since the port completed, every core commit
has been Holt-driven and no upstream sync has happened or is planned.

## Decisions

- **The core is a workspace member at `crates/core`, imported with
  `git subtree add` so its full history rides Holt's.** The package name
  stays `pi-core-rs` and the lib name `pi_core`, so no `use` site changes;
  `Cargo.toml`/`Cargo.lock` carry no git dependency on it anymore.
- **Upstream pi is no longer tracked.** The TypeScript oracle checkout, its
  tooling (`oracle/`, `scripts/`), the port tracker (`MIGRATION.md`), and the
  porting-workflow docs are dropped. The v0.84.4 behavior stays pinned by
  the ported test suite and the frozen goldens under `tests/goldens/`; a
  golden changes only with a deliberate behavior change.
- **The crate keeps its MIT license** (the license of the pi packages it was
  ported from): `crates/core/LICENSE` stays and its `Cargo.toml` declares
  `license = "MIT"` explicitly rather than inheriting the workspace's
  GPL-3.0.
- **The crate stays free of Holt domain concepts** (Space, Turn change set,
  …). `crates/engine` remains the only adapter; the day a different engine
  replaces it, the core does not change.
- **Core integration tests link as one target.** The former 90 `tests/*.rs`
  files are modules of `tests/all/main.rs` (shared helpers in
  `tests/all/common/`), so a core touch relinks one test binary instead of
  90. New integration tests join as modules, never as new top-level files.
- **No test may depend on the developer's process environment.** Env-reading
  tests hold `ai::test_env_lock()` and mask ambient provider credentials
  (`mask_ambient`); the credential-gated live suites (`ai_live_*`) keep
  their upstream `skipIf` semantics and fire real requests when credentials
  exist — `PI_TEST_OFFLINE=1` forces them all to skip.

## Consequences

A core fix and its engine/UI consumer land in one commit; editing core
in-tree is the normal workflow (the dev profile keeps incremental on and
caps core's debuginfo at line tables). `cargo test --workspace` now compiles
and runs the core's ~1,266 tests, including in CI — first run, not a
recompile regression: as a git dependency its tests never built here. Holt's
dependency graph briefly keeps both sha2/hmac major versions the two
repositories chose independently; converging them is follow-up, not part of
the move. The old `Onion-L/pi-core-rs` repository is archived to keep one
source of truth.
