# Holt

AGENTS.md is the manual for working in this repository: where to read, and
the rules for writing, updating, and deleting. What this project **is** —
topology, crates, hard fences — is [ARCHITECTURE.md](ARCHITECTURE.md)'s
first section; start there before touching `crates/rpc`, `crates/engine`,
or the boot path.

## Reading

| Question | Home |
| --- | --- |
| What is this project? Topology, hard fences | [ARCHITECTURE.md](ARCHITECTURE.md) |
| How does X run today? | [docs/subsystems/](docs/subsystems/README.md) — one page per subsystem, three groups; its README is the entry and holds the full page rules |
| Why was X decided, and what was rejected? | [docs/DECISION.md](docs/DECISION.md) — the decision log, anchored to landing commits |
| What does this word mean? | [CONTEXT.md](CONTEXT.md) |
| What went wrong before, and what would have caught it? | [docs/RETROS.md](docs/RETROS.md) |
| What did the surveys find? | [docs/research/](docs/research/) — point-in-time evidence behind decisions |
| The gpui API truth | [docs/research/gpui.md](docs/research/gpui.md) and the vendored sources — never online docs, never git |

Reading discipline: before changing a documented subsystem, read its page
and verify the linked sources against the code you are about to edit. A
page's Verification commands must actually run.

## Writing rules

These live here because an agent that skipped every other document must
still not break them.

- **Update the owning subsystem page in the same PR** when a change alters
  a documented responsibility, interface, state transition, runtime path,
  compatibility rule, or failure behavior. A refactor that preserves those
  contracts needs no prose change.
- **A new subsystem page** is earned by a behavior domain an implementer
  must cross crate boundaries to follow, with a runtime path or state of
  its own. A single module's behavior that its source file already explains
  extends a neighboring page or stays in comments.
- **A new decision-log entry** only when the decision names a rejected
  alternative, spans modules or a serialized format, or someone might
  relitigate it — otherwise it belongs in a code comment. The log is
  append-only: a changed decision appends a new entry and leaves an inline
  superseded line on the old one; entries are never edited in place.
- **One fact, one home.** If a fact appears in two places, one of them is
  wrong — fix the wrong one; do not keep them in sync. Prose that duplicates
  what code or another page already says gets deleted, not paraphrased.
- A lesson lands in [docs/RETROS.md](docs/RETROS.md) when merged work got
  something wrong that a rule would have caught: one entry naming the rule,
  the why, and the evidence.

## Deletion rules

- **A subsystem page dies with its subsystem**, in the same PR as the
  removal — pages for removed behavior are deleted, not archived.
- **DECISION.md entries are never deleted**, even when superseded; the
  supersession line is the tombstone.
- **A research note with no landing** — no decision entry cites it and
  nothing references it — is deleted rather than kept.
- When deleting or renaming any document, fix every reference to it in the
  same commit; grep for the old name.

## Commands

- Run: `cargo run --release -p holt` (headed only; no CLI). Data lives under
  `~/.holt`; override with `HOLT_DATA_DIR`.
- Check / lint: `cargo check --workspace`, `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`.
- Test: `cargo test --workspace`; focus with `-p`. `crates/core` live
  provider suites skip unless `PI_TEST_LIVE=1`. CI (macOS) gates on
  `cargo test --workspace` only.

## Facts with no other home

- Commits follow `type(crate): summary` (e.g. `feat(ui): …`); scopes in
  use: core, engine, ui — comma-joined when a change spans several.
- Releases: a pushed `v*` tag must match the workspace `version` in the
  root `Cargo.toml` (the Release workflow fails closed). A release bumps
  the version, adds the `site/changelog.html` entry, then commits
  `chore: bump workspace version to X.Y.Z`, tags, and pushes both.
- Issues are GitHub issues, filed through the `issue` skill; there is no
  local tracker.
