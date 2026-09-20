# Contributing to Holt

Thanks for your interest! Holt is an experimental project under active
development — things move fast and break occasionally, but contributions are
welcome. Please note that this project follows a
[Code of Conduct](CODE_OF_CONDUCT.md).

## Before You Start

- For bugs and feature requests, open a
  [GitHub issue](https://github.com/Onion-L/holt/issues). (The Markdown files
  under `.scratch/` are the maintainer's internal tracker, not the public one.)
- Read [ARCHITECTURE.md](ARCHITECTURE.md) before touching `crates/rpc`,
  `crates/engine`, or the boot path — it is the source of truth for crate
  topology and the RPC contract.
- For anything larger than a small fix, open an issue first so we can agree on
  the direction before you invest the time.

## Development Setup

Holt builds on stable Rust. The repo pins the toolchain in
`rust-toolchain.toml`, so `rustup` selects it automatically.

```bash
cargo run --release -p holt
```

Data lives under `~/.holt` (override with `HOLT_DATA_DIR`). To try a model run,
configure a provider API key in **Settings → Providers**.

## Checks

CI runs on macOS and requires all of the following to pass:

```bash
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Add or update focused tests when you change behavior; do not weaken validation
or error handling just to make a test pass.

## Project Conventions

- **The RPC boundary is real.** The UI never links backend logic; it talks the
  typed contract in `crates/rpc`. `crates/ui` is agent-agnostic — it renders
  `MessagePart`s from `holt-doc`, never raw agent events.
- **The engine slice is intentionally narrow.** Worktrees, change requests,
  and uploads are rendered by the UI but unserved — those RPCs reply
  `UnknownMethod` by design, not regression.
- **gpui is a frozen vendored snapshot** under `vendor/gpui`. Edit it in place
  when needed; never resolve gpui from git, and check the vendored sources
  rather than online docs for its API.
- **Commits** follow `type(crate): summary` — e.g. `feat(ui): …`,
  `fix(engine): …`. Keep each commit focused on one change.

## Pull Requests

- Keep PRs small and scoped; one behavior change per PR.
- Describe the user-visible behavior and how you verified it.
- Make sure the checks above pass before requesting review.

## Releases

Pushing a `v*` tag runs the Release workflow. The tag must match the
`workspace.version` in `Cargo.toml` — the workflow fails closed on a mismatch,
so the release procedure is:

1. Bump `workspace.version` in `Cargo.toml`, commit.
2. `git tag v<version> && git push origin v<version>`.

If you forget the bump, the workflow fails before building anything; delete
the tag, bump, and re-tag. A tag whose release already exists is refused.

The workflow tests, then builds, signs, and notarizes two DMGs (arm64, x64)
and publishes them to a GitHub Release with generated notes. It needs these
repository secrets: `APPLE_CERTIFICATE_P12` / `APPLE_CERTIFICATE_PASSWORD`
(Developer ID certificate, P12 base64-encoded) and `APPLE_API_KEY_P8` /
`APPLE_API_KEY_ID` / `APPLE_API_ISSUER_ID` (App Store Connect API key for
notarization). Locally, `scripts/build-dmg.sh` does the same build against
your own keychain.

## License

By contributing, you agree that your contributions are licensed under the
terms of the [LICENSE](LICENSE) file.
