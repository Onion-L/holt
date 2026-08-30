# Holt

Rust desktop UI shell for a coding agent. Frontend only: `crates/engine` is a
stub backend slot awaiting the from-scratch Rust agent core (pi-core-rs).
`ARCHITECTURE.md` is the source of truth for crate topology and the RPC
contract — read it before touching `crates/rpc`, `crates/engine`, or the boot
path.

## Commands

- Run: `cargo run --release -p holt` (headed only; no CLI).
- Data lives under `~/.holt`; override with `HOLT_DATA_DIR`.
- Check / lint: `cargo check --workspace`, `cargo clippy --workspace`,
  `cargo fmt --all`.
- Test: `cargo test --workspace`; focus with `-p`, e.g. `cargo test -p holt-doc`.
- The running app is an empty shell by design: backend mutations fail until a
  real engine is wired in. That is the stub, not a regression.

## Architecture rules

- The UI never links backend logic. It talks the typed RPC contract in
  `crates/rpc` over the in-process memory transport; `rpc::methods` is the
  full UI↔backend surface. A real backend implements `RpcService` and replaces
  `StubEngine` behind the same trait.
- `crates/ui` is agent-agnostic: it renders `MessagePart`s from `holt-doc`,
  never raw agent events.

## Gotchas

- gpui is a frozen vendored snapshot under `vendor/gpui` (excluded from the
  workspace; carries the glass/edge-fade patches the UI depends on). Edit it in
  place when needed; never resolve gpui from git, and don't guess its pre-1.0
  API from online docs — `docs/research/gpui.md` and the vendored sources are
  the API truth.
- This workspace is not a git repository: no history, no branches, nothing to
  diff against.

## Agent skills

### Issue tracker

Issues are tracked as local Markdown files under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Triage labels

The default five-role triage vocabulary is used. See `docs/agents/triage-labels.md`.

### Domain docs

This repo uses a single-context layout. See `docs/agents/domain.md`.
