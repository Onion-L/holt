# Holt — Architecture

A desktop-only UI shell. The frontend talks to a small in-process engine; its
first real capability is the `pi-core-rs` agent loop. See README.md for what
was removed.

## Topology

```
gpui UI ── in-memory RPC (ndjson envelopes) ── StubEngine + pi-core agent loop
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
| `crates/engine` | The backend adapter. `StubEngine` serves the current in-memory chat/session/transcript runtime, discovers built-in providers and models through `pi-core-rs`, owns credential persistence, and runs `pi-core-rs::agent_loop`. Unsupported surfaces still return empty watches or unknown-method replies. |
| `crates/rpc` | The typed control plane: framing, `RpcClient` (call/subscribe), `RpcService` dispatch, memory transport. Method names live in `rpc::methods` — that module is the full UI↔backend contract. |
| `crates/proto` | Shared types: `ProviderId`, provider-qualified models and run configuration, entities (Chat/Space/Device/Session), `EngineInfo`, and view derivations. |
| `crates/doc` | Loro-CRDT session docs and the `MessagePart`/`TranscriptFrame` types the transcript renders. |
| `crates/theme`, `crates/syntax` | Theme library and syntax highlighting. |

## The RPC contract (what a real backend must serve)

Defined by `crates/rpc/src/lib.rs::methods` and consumed by
`crates/ui/src/state.rs` (`attach_engine` starts the standing watches):

- Identity/barrier: `EngineInfo`, `EngineReady`.
- Entity watches: `WatchChats`, `WatchSpaces`, `WatchSessions`
  (each emits `Vec<T>` snapshots), `WatchConnectivity`, `WatchTransfers`.
- Provider configuration: `ListProviders`, `SaveProviderKey`,
  `RevealProviderKey`, `RemoveProviderKey`, `AddProviderModel`.
- Catalog: provider-scoped `ListModels`, plus `ListCommands`.
- Transcript: `WatchDocMessages` (`TranscriptFrame` stream per chat).
- Mutations: `Mutate` (createChat/createSpace/…), `QueueCommand`.
- Capability surfaces the UI keeps rendered but the stub leaves empty:
  terminals, repos/worktrees/diffs, and uploads.

Reply shapes are serialized camelCase; the UI parses tolerantly and skips
methods that error with `UnknownMethod`.

## Boot path

`main` → `holt_ui::run_app(UiConfig { data_dir, initial_url })` →
`AppState::bootstrap` → `EngineHandle::bootstrap` assembles the engine (device
id + instance lock under `~/.holt`) and connects a memory `RpcClient`. The boot
gate resolves immediately: local scope + ready connection ⇒ no sign-in wall.

Provider credentials live in `provider-credentials.json` under the Holt data
directory. Writes are atomic, Unix permissions are `0600`, malformed files fail
startup, and credentials enter the agent loop as per-request snapshots. The UI
only sees secrets through the dedicated reveal RPC.

Provider availability is derived from credentials alone: a provider is
offered once its key is configured; there is no separate enable toggle.
`ListProviders` groups sibling built-ins that share a `pi-core-rs`
`organization_id` (e.g. `minimax` + `minimax-cn`) into one row per
organization; the row's `variants` carry the concrete provider ids that the
key, model-list, and run RPCs address. Provider-scoped custom model IDs added
from Settings live in `provider-settings.json`; they are merged into
`ListModels` and resolved through the provider's existing API transport.

The implemented agent slice is intentionally narrow: provider configuration,
provider/model discovery, `createChat`, chat/session watches, `QueueCommand`
run/interrupt, and streamed transcript frames. The run loop mounts pi-core's
built-in read/write/edit/bash tools (via `engine::tools`, a local
`ExecutionEnv` rooted at the chat's cwd); the transcript folds their calls
and results into `MessagePart::Tool` chips. Durable sessions, steering,
worktrees, and uploads remain outside this slice.

## Provenance notes

- gpui is vendored under `vendor/gpui/` (25 crates + `tooling/perf`, its own
  trimmed workspace root; `vendor/` additionally holds the xim-rs / font-kit /
  scap sources gpui's platform backends need) — a snapshot of a zed fork
  carrying the glass/edge-fade patches the UI depends on. It is a frozen
  asset: edit it in place when needed; no dependency resolves from git.
- `THIRD_PARTY_NOTICES.md` carries upstream attribution obligations.
- Historical design docs for removed subsystems (sync, agent drivers, edge)
  were deleted with them; `docs/` keeps UI/theme/gpui/memory references.
