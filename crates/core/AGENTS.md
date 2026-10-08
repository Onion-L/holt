# crates/core

Holt's agent core (package `pi-core-rs`, lib `pi_core`): a Rust port of
`@earendil-works/pi-agent-core`, `pi-ai`, and `pi-telemetry` v0.84.4,
originally developed as the standalone `pi-core-rs` repository and merged
into this workspace (ADR-0043). MIT-licensed: keep `LICENSE` and the explicit
`license = "MIT"` in `Cargo.toml` — never inherit the workspace's GPL-3.0.

Holt owns this crate. Upstream pi is no longer tracked: the v0.84.4
TypeScript sources were the behavioral oracle during the port, and that
oracle, its tooling, and the migration tracker are gone. Preserve existing
observable behavior — agent event order and payloads, state transitions,
session JSONL format, tool names/schemas/output text, compaction and
branch-summary results, retry and cancellation behavior, provider stream
normalization, usage accounting — unless a change is deliberate; record
deliverable deviations from upstream pi in the commit message.

Keep the crate free of Holt domain concepts (Space, Turn change set, …):
`crates/engine` is the only adapter.

## Layout

- `src/` — implementation. Module paths mirror the former TypeScript
  packages (`telemetry`, `ai`, `agent`).
- `src/bin/pi-ai.rs` — OAuth helper binary (`pi-ai`).
- `tests/all/` — the single integration test target: one module per former
  `tests/*.rs` file, shared helpers in `tests/all/common/`. Add new
  integration tests as a module there, never as a new top-level `tests/*.rs`
  file (each would become its own linked binary again).
- `tests/goldens/` — serialized-output fixtures produced by the TypeScript
  oracle during the port. Frozen: a golden changes only with a deliberate
  behavior change, recorded in the commit message.

## Testing

Credential-gated live suites (`ai_live_*`) fire real provider requests
whenever their env credentials exist — exporting `ANTHROPIC_API_KEY` locally
runs the Anthropic live suite against the real API. Export
`PI_TEST_OFFLINE=1` to force every live suite to skip (CI runs without
credentials, so this only matters on a developer machine).

Tests that touch provider env vars hold `ai::test_env_lock()` and mask
ambient vars (`mask_ambient` in `src/ai/env_api_keys.rs`); assertions must
never depend on the developer's process environment.

Verify from the workspace root:

```bash
cargo test -p pi-core-rs
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```
