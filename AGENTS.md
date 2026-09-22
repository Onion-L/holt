# Holt

Rust desktop UI shell for a coding agent. `crates/engine` runs the `pi-core-rs` agent loop
behind the `RpcService` trait. `ARCHITECTURE.md` is the source of truth for crate topology
and the RPC contract — read it before touching `crates/rpc`, `crates/engine`, or the boot path.

## Commands

- Run: `cargo run --release -p holt` (headed only; no CLI). Data lives under
  `~/.holt`; override with `HOLT_DATA_DIR`.
- Check / lint: `cargo check --workspace`, `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`.
- Test: `cargo test --workspace`; focus with `-p`, e.g. `cargo test -p holt-doc`.
- CI runs on macOS and gates on check + test only (`.github/workflows/ci.yml`).

## Architecture rules

- The UI never links backend logic. It talks the typed RPC contract in `crates/rpc` over the
  in-process memory transport; `rpc::methods` is the full UI↔backend surface. `LocalEngine` in
  `crates/engine` serves it today; a different backend can slot in behind the same trait.
- The engine slice is intentionally narrow (see "The RPC contract" in `ARCHITECTURE.md`):
  worktrees, change requests, and uploads are rendered by the UI but unserved — those RPCs reply
  `UnknownMethod` by design, not regression. The git surface (branches, checkout diffs in all
  four modes, history, fetch) IS served on the git2 backend.
- `crates/ui` is agent-agnostic: it renders `MessagePart`s from `holt-doc`, never raw agent events.
- Git2 access lives in `crates/engine/src/git.rs` (ADR-0001).

## Code style

- Check `Cargo.toml` and existing imports before adding a dependency; prefer
  existing helpers and types over parallel abstractions.
- Keep async cancellation and lock boundaries consistent with neighboring code.
- Do not change public types, RPC methods, or serialized representations
  without checking every consumer and `ARCHITECTURE.md`.

## Gotchas

- gpui is a frozen vendored snapshot under `vendor/gpui` (excluded from the
  workspace; carries the glass/edge-fade patches the UI depends on). Edit it
  in place; never resolve gpui from git or guess its pre-1.0 API from online
  docs — `docs/research/gpui.md` and the vendored sources are the API truth.
- GPUI nested scroll containers do not contain wheel events automatically: `.overflow_y_scroll()`
  inside a `List` must use `.occlude()` or one wheel gesture can move both the child and the outer
  list. A bubble-phase `.on_scroll_wheel()` handler is insufficient because the outer List listener
  may run first. See ADR-0013.
- The agent loop itself lives in the external `pi-core-rs` crate (git dependency in root
  `Cargo.toml`); `crates/engine` only adapts it — run wiring, tools via `engine::tools`,
  credentials, provider settings.
- Commits follow `type(crate): summary` (e.g. `feat(ui): …`); scopes in use:
  engine, ui, or `engine,ui` when a change spans both.
- Releases: a pushed `v*` tag must match `workspace.version` in the root
  `Cargo.toml`; the Release workflow fails closed on a mismatch.

## Agent skills

- Issues are local Markdown under `.scratch/` (`docs/agents/issue-tracker.md`);
  triage vocabulary: `docs/agents/triage-labels.md`.
- Domain docs are single-context: read `CONTEXT.md` and the `docs/adr/`
  entries touching your area before exploring (`docs/agents/domain.md`).
