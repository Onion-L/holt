# Holt

A desktop coding agent built around a Rust UI and an in-process Rust agent loop:

- Holt is the only coding agent. Model services are configured as providers;
  local coding-agent CLIs are not interchangeable runtimes.
- No cloud — edge worker, multi-device sync, accounts/auth, updates, and the
  iOS/landing apps are deleted.
- No daemon/CLI — the app is headed-only and embeds its backend in-process
  over the memory RPC transport.

`crates/engine` adapts `pi-core-rs` behind Holt's typed `RpcService` boundary.

## Run

```bash
cargo run --release -p holt
```

Data lives under `~/.holt` (override with `HOLT_DATA_DIR`). Configure a provider
API key in Settings → Providers before starting a model run.

## Layout

See [ARCHITECTURE.md](ARCHITECTURE.md).
