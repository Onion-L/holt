# Holt — Architecture

A desktop-only UI shell. The frontend is intact; the backend is a stub slot
awaiting a real engine (pi-core-rs). See README.md for what was removed.

## Topology

```
gpui UI ── in-memory RPC (ndjson envelopes) ── StubEngine (crates/engine)
```

One binary, headed only. The UI never links backend logic directly: it talks
the typed RPC contract in `crates/rpc` over an in-process duplex
(`holt_rpc::memory_client`). A real backend replaces the stub behind the same
`RpcService` trait and the UI lights up without further changes.

## Crates

| Crate | Role |
| --- | --- |
| `apps/holt` | The binary: logging setup + `holt_ui::run_app`. No CLI. |
| `crates/ui` | The whole gpui viewport (~69k lines): shell, sidebar, transcript, composer, terminal/diff panes, settings, themes. Agent-agnostic — it renders `MessagePart`s from `holt-doc`, never raw agent events. |
| `crates/engine` | **The backend slot.** `StubEngine` answers the RPC surface with empty catalogs/watches and unknown-method replies for everything it has no backend for; `InstanceLock` guards the data dir; `registry` keeps the harness-descriptor types the settings UI reads. |
| `crates/rpc` | The typed control plane: framing, `RpcClient` (call/subscribe), `RpcService` dispatch, memory transport. Method names live in `rpc::methods` — that module is the full UI↔backend contract. |
| `crates/proto` | Shared types: `HarnessId`, entities (Chat/Space/Device/Session), `EngineInfo`, view derivations (sort/gate/staleness). |
| `crates/doc` | Loro-CRDT session docs and the `MessagePart`/`TranscriptFrame` types the transcript renders. |
| `crates/theme`, `crates/syntax` | Theme library and syntax highlighting. |

## The RPC contract (what a real backend must serve)

Defined by `crates/rpc/src/lib.rs::methods` and consumed by
`crates/ui/src/state.rs` (`attach_engine` starts the standing watches):

- Identity/barrier: `EngineInfo`, `EngineReady`.
- Entity watches: `WatchChats`, `WatchSpaces`, `WatchSessions`
  (each emits `Vec<T>` snapshots), `WatchConnectivity`, `WatchTransfers`.
- Catalog: `ListHarnesses`, `ListModels`, `ListCommands`.
- Transcript: `WatchDocMessages` (`TranscriptFrame` stream per chat).
- Mutations: `Mutate` (createChat/createSpace/…), `QueueCommand`.
- Capability surfaces the UI keeps rendered but the stub leaves empty:
  terminals, repos/worktrees/diffs, uploads, agent accounts.

Reply shapes are serialized camelCase; the UI parses tolerantly and skips
methods that error with `UnknownMethod`.

## Boot path

`main` → `holt_ui::run_app(UiConfig { data_dir, initial_url })` →
`AppState::bootstrap` → `EngineHandle::bootstrap` assembles the stub (device id
+ instance lock under `~/.holt`) and connects a memory `RpcClient`. The boot
gate resolves immediately: local scope + ready connection ⇒ no sign-in wall.

## Provenance notes

- gpui is vendored under `vendor/gpui/` (25 crates + `tooling/perf`, its own
  trimmed workspace root; `vendor/` additionally holds the xim-rs / font-kit /
  scap sources gpui's platform backends need) — a snapshot of a zed fork
  carrying the glass/edge-fade patches the UI depends on. It is a frozen
  asset: edit it in place when needed; no dependency resolves from git.
- `THIRD_PARTY_NOTICES.md` carries upstream attribution obligations.
- Historical design docs for removed subsystems (sync, harness drivers, edge)
  were deleted with them; `docs/` keeps UI/theme/gpui/memory references.
