# Holt

A desktop UI shell for a coding agent — **desktop frontend only**:

- No agent drivers — the claude-code / codex / ACP / opencode / cursor harness
  layer is gone. `crates/engine` is now a stub backend slot.
- No cloud — edge worker, multi-device sync, accounts/auth, updates, and the
  iOS/landing apps are deleted.
- No daemon/CLI — the app is headed-only and embeds its backend in-process
  over the memory RPC transport.

The intended backend is a from-scratch Rust agent core (pi-core-rs); it plugs
in by implementing the `RpcService` method surface in `crates/engine`.

## Run

```bash
cargo run --release -p holt
```

Data lives under `~/.holt` (override with `HOLT_DATA_DIR`). Expect an empty
shell: no chats, no agents, and backend mutations fail until a real engine is
wired in.

## Layout

See [ARCHITECTURE.md](ARCHITECTURE.md).
