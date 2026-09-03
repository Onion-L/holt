# Holt

Rust desktop UI shell for a coding agent. The engine is real but narrow:
`crates/engine` runs the `pi-core-rs` agent loop behind the `RpcService`
trait. `ARCHITECTURE.md` is the source of truth for crate topology and the RPC
contract — read it before touching `crates/rpc`, `crates/engine`, or the boot
path.

## Commands

- Run: `cargo run --release -p holt` (headed only; no CLI).
- Data lives under `~/.holt`; override with `HOLT_DATA_DIR`.
- Check / lint: `cargo check --workspace`, `cargo clippy --workspace`,
  `cargo fmt --all`.
- Test: `cargo test --workspace`; focus with `-p`, e.g. `cargo test -p holt-doc`.
- The engine slice is intentionally narrow (see "The RPC contract" in
  `ARCHITECTURE.md`): terminals, worktrees, change requests, and uploads
  are rendered by the UI but unserved — those RPCs reply `UnknownMethod` by
  design, not regression. The git surface (branches, checkout diffs in all
  four modes, history, fetch) IS served on the git2 backend; all git2
  access lives in `crates/engine/src/git.rs` (ADR-0001).

## Architecture rules

- The UI never links backend logic. It talks the typed RPC contract in
  `crates/rpc` over the in-process memory transport; `rpc::methods` is the
  full UI↔backend surface. `LocalEngine` in `crates/engine` serves it today; a
  different backend can slot in behind the same `RpcService` trait.
- `crates/ui` is agent-agnostic: it renders `MessagePart`s from `holt-doc`,
  never raw agent events.

## Gotchas

- gpui is a frozen vendored snapshot under `vendor/gpui` (excluded from the
  workspace; carries the glass/edge-fade patches the UI depends on). Edit it in
  place when needed; never resolve gpui from git, and don't guess its pre-1.0
  API from online docs — `docs/research/gpui.md` and the vendored sources are
  the API truth.
- The agent loop itself lives in the external `pi-core-rs` crate (git
  dependency in the root `Cargo.toml`); `crates/engine` only adapts it — run
  wiring, tools via `engine::tools`, credentials, provider settings.
- Commits follow `type(crate): summary` (e.g. `feat(ui): …`); scopes in use:
  engine, ui, settings.

## Agent skills

### Issue tracker

Issues are tracked as local Markdown files under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Triage labels

The default five-role triage vocabulary is used. See `docs/agents/triage-labels.md`.

### Domain docs

This repo uses a single-context layout. See `docs/agents/domain.md`.
