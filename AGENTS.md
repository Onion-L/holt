# Holt

Rust desktop UI shell for a coding agent. `crates/engine` runs the `pi-core-rs` agent loop
behind the `RpcService` trait.

## Documentation

| Question | Home |
| --- | --- |
| How does X run today? | `docs/subsystems/` — one page per subsystem, three groups |
| Why was X decided, and what was rejected? | `docs/DECISION.md` (+ surveys under `docs/research/`) |
| What does this word mean? | `CONTEXT.md` |
| Topology and hard fences | `ARCHITECTURE.md` |

Before changing a documented subsystem, read its page and verify the linked
sources against the code you are about to edit. A change that alters a
documented responsibility, interface, state transition, runtime path,
compatibility rule, or failure behavior updates the owning page **in the same
PR**; a refactor that preserves them needs no prose change. Full rules:
`docs/subsystems/README.md`.

A new decision earns a `docs/DECISION.md` entry only if it names a rejected
alternative, spans modules or a serialized format, or someone might
relitigate it — otherwise it belongs in a code comment.

## Commands

- Run: `cargo run --release -p holt` (headed only; no CLI). Data lives under
  `~/.holt`; override with `HOLT_DATA_DIR`.
- Check / lint: `cargo check --workspace`, `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`.
- Test: `cargo test --workspace`; focus with `-p`, e.g. `cargo test -p holt-doc`.
  `crates/core` live provider suites skip unless `PI_TEST_LIVE=1`.
- CI runs on macOS and gates on `cargo test --workspace` only (`.github/workflows/ci.yml`).

## Architecture rules

- The UI never links backend logic. It talks the typed RPC contract in `crates/rpc` over the
  in-process memory transport; `rpc::methods` is the full UI↔backend surface. `LocalEngine` in
  `crates/engine` serves it today; a different backend can slot in behind the same trait.
- The engine slice is intentionally narrow (see "The RPC surface" in `ARCHITECTURE.md`):
  worktrees, change requests, and uploads are rendered by the UI but unserved — those RPCs reply
  `UnknownMethod` by design, not regression. The git surface (branches, checkout diffs in all
  four modes, history, fetch) IS served on the git2 backend.
- `crates/ui` is agent-agnostic: it renders `MessagePart`s from `holt-doc`, never raw agent events.
- Git2 access lives only in `crates/engine/src/git.rs` (decision:
  git2-as-the-git-backend); the workspace filesystem lives only in
  `crates/engine/src/files` — and the UI never touches the user's repository
  or working files.

## Code style

- Check `Cargo.toml` and existing imports before adding a dependency; prefer
  existing helpers and types over parallel abstractions.
- Keep async cancellation and lock boundaries consistent with neighboring code.
- Do not change public types, RPC methods, or serialized representations
  without checking every consumer and `ARCHITECTURE.md`.

## Gotchas

- gpui is a frozen third-party snapshot under `third_party/gpui` (excluded from the
  workspace; carries the glass/edge-fade patches the UI depends on). Edit it
  in place; never resolve gpui from git or guess its pre-1.0 API from online
  docs — `docs/research/gpui.md` and the vendored sources are the API truth.
- GPUI nested scroll containers do not contain wheel events automatically:
  a scrollable inside a `List` must `.occlude()` — a bubble-phase wheel
  handler is insufficient (decision: nested-scroll-containers-isolate-wheel-events).
- The agent loop itself lives in `crates/core` (package `pi-core-rs`, lib
  `pi_core`; merged in as a workspace member, decision agent-core-is-a-holt-owned-workspace-crate); `crates/engine`
  only adapts it — run wiring, tools via `engine::tools`, credentials,
  provider settings.
- Commits follow `type(crate): summary` (e.g. `feat(ui): …`); scopes in use:
  core, engine, ui, comma-joined when a change spans several (e.g. `engine,ui`).
- Releases: a pushed `v*` tag must match `workspace.version` in the root
  `Cargo.toml`; the Release workflow fails closed on a mismatch. A release
  touches three things — bump `workspace.version` (any cargo command refreshes
  `Cargo.lock`), add the `site/changelog.html` entry and move its `current`
  stamp (entries track `git log v<prev>..v<next> --oneline`), then commit
  `chore: bump workspace version to X.Y.Z`, tag `vX.Y.Z`, and push both;
  the site redeploys from `main`.
- Issues are GitHub issues, filed through the `issue` skill; there is no local
  tracker.
