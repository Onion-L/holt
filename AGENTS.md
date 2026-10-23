# Holt

AGENTS.md is the manual for working in this repository. The general sections
come first — each points at one document an agent must read before the
relevant work; the one domain section after them carries the knowledge that
is easiest to waste work on. What this project **is** starts in
[ARCHITECTURE.md](ARCHITECTURE.md).

## Architecture and vocabulary

[ARCHITECTURE.md](ARCHITECTURE.md) holds the topology, the four hard
boundaries (UI talks only `rpc::methods`; `engine` is `core`'s only adapter;
git2 only in `engine::git` and the workspace filesystem only in
`engine::files`; all disk writes are engine-owned), and the RPC-surface map
to the owning pages. Terms live in [CONTEXT.md](CONTEXT.md); the gpui API
truth is [docs/research/gpui.md](docs/research/gpui.md) and the vendored
sources — never online docs, never git.

## Subsystem design and review

Three reading triggers, all defined in the [subsystems README](docs/subsystems/README.md):
**before changing code** — read the task's page and follow its source links
against the implementation you are about to edit; **before preparing a PR**
— the description gives each affected subsystem's observable change,
trigger, and before/after; **when reviewing one** — identify the affected
subsystem and the actual runtime path, and check the page and its links.
The same README's deletion rules apply to pages that lose their subsystem.

## Code standards

Write and review changes against [docs/CODE-STANDARDS.md](docs/CODE-STANDARDS.md).
It grows by one mechanism only: a RETROS.md lesson seen on three separate
days is promoted into it.

## Decisions

Before changing code, read the [docs/DECISION.md](docs/DECISION.md) entries
its index lists under your area's subsystem page: they record why it is the
way it is and what was rejected. A new entry is earned only by a decision that names a rejected
alternative, spans modules or a serialized format, or someone might
relitigate — otherwise it belongs in a code comment. The log is append-only:
a changed decision appends a new entry and leaves an inline superseded line
on the old one.

## Lessons

Before changing code, read [docs/RETROS.md](docs/RETROS.md) — what recent
merged work got wrong and the rule that would have caught it. Entries are
made by the retro review (fixed twice / should not have merged / verified
too thinly) or by an author who catches their own miss; the format, the
seen-counter, and the three-strikes promotion into this file's rules are
the RETROS header's to define.

## What the engine actually serves

The UI renders more than the engine serves. Changing UI code against an
unserved RPC, or "fixing" an `UnknownMethod` reply, is work on code that
does not run.

- **Served**: the whole chat/space/session surface, the queue and Turn loop,
  provider configuration, the git surface (branches, all four diff modes,
  history, fetch), the file sidebar, terminals, usage, Turn change sets,
  plan/provider/goal modes, routines, self-update.
- **Deliberately unserved** (the UI keeps rendering them; `UnknownMethod`
  is the contract, not a regression): the sync and account surface —
  sign-in, orgs, repos, clone, uploads — and worktree lifecycle beyond
  creation-on-demand.
- The per-method contract for anything served lives in its owning
  subsystem page; the method names themselves live in `rpc::methods`.

## Commands

- Run: `cargo run -p holt` (headed only; no CLI; debug build — a release
  build's binary conflicts with an installed holt). Data lives under
  `~/.holt`; override with `HOLT_DATA_DIR`.
- Check / lint: `cargo check --workspace`, `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`.
- Test: `cargo test --workspace`; focus with `-p`. `crates/core` live
  provider suites skip unless `HOLT_TEST_LIVE=1`. CI (macOS, Linux,
  Windows) gates on `cargo test --workspace` only.

## Facts with no other home

- Commits follow `type(crate): summary` (e.g. `feat(ui): …`); scopes in
  use: core, engine, ui — comma-joined when a change spans several.
- Releases: a pushed `v*` tag must match the workspace `version` in the
  root `Cargo.toml` (the Release workflow fails closed). A release bumps
  the version, adds the `site/src/pages/changelog.astro` entry, then
  commits `chore: bump workspace version to X.Y.Z`, tags, and pushes both.
  A tag with a semver pre-release suffix (`vX.Y.Z-alpha.1`) publishes as a
  GitHub prerelease without flipping latest — the download page keeps
  serving the last stable release. The Release workflow runs over 20
  minutes — push and move on; never block a session watching it.
- Issues are GitHub issues, filed through the `issue` skill; there is no
  local tracker.
