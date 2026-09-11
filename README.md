<p align="center">
  <img src="crates/ui/assets/app-icon.png" alt="Holt app icon" width="144" />
</p>

<h1 align="center">Holt</h1>

<p align="center">
  A native desktop coding agent built with Rust and GPUI.
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: GPL-3.0" src="https://img.shields.io/badge/license-GPL--3.0-blue" /></a>
  <img alt="Rust stable" src="https://img.shields.io/badge/rust-stable-orange" />
</p>

<p align="center">
  <img src="docs/assets/screenshot.png" alt="Holt — transcript alongside a branch-changes diff" width="100%" />
</p>

> [!WARNING]
> Holt is a work in progress — an experimental project under active
> development. Expect rough edges and breaking changes.

## Why Holt

Why not.

Holt is the agent. Model services plug in as providers; local coding-agent
CLIs are not interchangeable runtimes. One coherent agent loop drives every
chat. No cloud. No accounts, no sync, no telemetry. Chats, transcripts, and
credentials live under `~/.holt` and nowhere else. No daemon, no CLI. A headed
desktop app that embeds its backend in-process over a typed memory-RPC
transport.

## Quick start

```bash
cargo run --release -p holt
```

Then open **Settings → Providers** and add an API key before starting a run.

Data lives under `~/.holt`; override with `HOLT_DATA_DIR`.

## Architecture

```
gpui UI ── in-memory RPC (ndjson envelopes) ── LocalEngine + core agent loop
```

The UI never links backend logic. It talks the typed contract in `crates/rpc`;
`crates/engine` adapts [pi-core-rs](https://github.com/Onion-L/pi-core-rs)
behind the `RpcService` trait, so another backend can slot in without touching
the UI.

| Crate | Role |
| --- | --- |
| `apps/holt` | The binary. No CLI. |
| `crates/ui` | The gpui viewport — agent-agnostic, renders `MessagePart`s from `holt-doc`. |
| `crates/engine` | The backend adapter: agent loop, providers, credentials, git, skills, terminals. |
| `crates/rpc` | The typed control plane: framing, dispatch, memory transport. |
| `crates/proto` | Shared protocol and domain types. |
| `crates/doc` | Loro-CRDT session docs and transcript types. |
| `crates/theme`, `crates/syntax` | Theme library and syntax highlighting. |

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full RPC contract and the design
decisions behind it.

## Development

```bash
cargo check --workspace
cargo clippy --workspace
cargo fmt --all
cargo test --workspace
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) — and please follow the
[Code of Conduct](CODE_OF_CONDUCT.md). Found a security issue? Report it
privately via [SECURITY.md](SECURITY.md).

## Credits

The beautiful UI comes from [comet](https://github.com/zeronsh/comet) ❤️
