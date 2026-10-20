# Holt — Architecture

A desktop-only UI shell. The frontend talks to a small in-process engine over a typed RPC contract; its first real capability is the `holt-core` agent loop. README.md covers the project overview and quick start.

This file owns topology, the hard boundaries, and the maps between them. How the system runs today lives in [docs/subsystems](docs/subsystems/README.md).

## Topology

```
gpui UI ── in-memory RPC (ndjson envelopes) ── LocalEngine + holt-core agent loop
```

One binary, headed only. The UI links `crates/engine` for exactly one thing — assembling the local backend at bootstrap (`EngineHandle::bootstrap` in `ui/state.rs`). No feature code in `crates/ui` calls backend logic: it talks the typed RPC contract in `crates/rpc` over an in-process duplex (`holt_rpc::memory_client`). The local backend implements the `RpcService` contract; a different backend can slot in behind the same trait.

## Crates

| Crate | Role |
| --- | --- |
| `apps/holt` | The binary: logging setup + `holt_ui::run_app`. No CLI. |
| `apps/holt-runner` | Headless Turn runner for the local eval harness; assembles `LocalEngine` in a fresh data dir and runs one full-access Turn through the RPC contract. Not packaged. |
| `crates/ui` | The whole gpui viewport. Agent-agnostic — it renders `MessagePart`s from `holt-doc`, never raw agent events. |
| `crates/engine` | The backend adapter: `LocalEngine` serves the RPC contract. Every subsystem page's "sources" rows live here. |
| `crates/rpc` | The typed control plane: framing, `RpcClient` (call/subscribe), `RpcService` dispatch, memory transport. Method names live in `rpc::methods` — the full UI↔backend surface. |
| `crates/proto` | Shared types: `ProviderId`, provider-qualified models and run configuration, entities (Chat/Space/Device/Session), view derivations, usage frames. |
| `crates/doc` | Wire types both ends exchange — `MessagePart`, `SessionMessageEntry`, `TranscriptFrame` — plus transcript-frame diffing. Persistence itself is engine-owned (see persistence). The crate's Loro session/workspace schemas and HLC registry port are dormant — nothing outside `crates/doc` links them. |
| `crates/theme`, `crates/syntax` | Theme library and syntax highlighting. |
| `crates/core` | The agent core (package `holt-core`, lib `holt_core`): provider adapters and streaming, the agent loop, the harness. Free of Holt domain concepts — `crates/engine` is the only adapter. MIT-licensed (decision: agent-core-is-a-holt-owned-workspace-crate). |

## Hard boundaries

Four fences every change must respect:

1. **UI ↔ backend goes only through `rpc::methods`.** The one exception is bootstrap assembly. Reply shapes are serialized camelCase; the UI parses tolerantly and skips methods that answer `UnknownMethod`.
2. **`crates/engine` is the only adapter of `crates/core`** — domain concepts never leak into the core.
3. **git2 access lives only in `engine::git`; the workspace filesystem lives only in `engine::files`** — the UI never touches the user's repository or working files, and neither does the agent outside its mounted tools.
4. **All disk writes are engine-owned** — atomic where a whole file is replaced, tolerant on read; per-file policies live in the owning subsystem page.

## The RPC surface

Method names live in `rpc::methods` (code is the contract). Each family's behavior — parameters, failure modes, lifecycles — lives in its owning page:

| Family | Selected methods | Owning page |
| --- | --- | --- |
| Identity, entity watches, mutations, self-update | `EngineInfo`, `WatchChats`/`Spaces`/`Sessions`, `Mutate` ops, `UpdateStatus` | [shell-registry](docs/subsystems/shell-registry.md) |
| The run: queue, message edits, terminal events, retries | `QueueCommand`, `EditLastMessage`, `WatchMessageQueue`, `WatchTurnTerminalEvents`, `WatchTurnRetry` | [conversation-policy](docs/subsystems/conversation-policy.md), [run-loop](docs/subsystems/run-loop.md) |
| Provider configuration and catalog writes | `ListProviders`, `SaveProviderKey`, `SaveModelRecord`, `ProbeProvider` | [provider-transport](docs/subsystems/provider-transport.md) |
| Permission modes, approvals, questions | `setChatPermissionMode`, `ResolveApproval`, `SettleQuestion` | [permission-gate](docs/subsystems/permission-gate.md), [conversation-policy](docs/subsystems/conversation-policy.md) |
| Plan and Provider modes, proposals, key requests | `EnterPlanMode`, `ApplyModelProposal`, `SettleProviderKeyRequest` | [modes](docs/subsystems/modes.md) |
| Goals and routines | `SetGoal`, `SetGoalPaused`, `ListRoutines`, `RunRoutineNow` | [goals-routines](docs/subsystems/goals-routines.md) |
| Catalog: models, commands, skills | `ListModels`, `ListCommands`, `ListSkills` | [prompts-skills](docs/subsystems/prompts-skills.md), [provider-transport](docs/subsystems/provider-transport.md) |
| Workspace files and images | `ListWorkspaceEntries`, `ReadWorkspaceFile`, `SaveWorkspaceFile`, `StageImage`, `SearchFiles` | [workspace-files](docs/subsystems/workspace-files.md) |
| Git and Turn change sets | `ListRefs`, `GetCheckoutDiff`, `GetTurnChangeSet`, `RestoreTurnChanges` | [workspace-git](docs/subsystems/workspace-git.md) |
| Terminals | `OpenTerminal`, `WriteTerminal`, `SubscribeTerminal`, `CloseTerminal` | [terminals](docs/subsystems/terminals.md) |
| Transcript and subagent blobs | `WatchDocMessages`, `FetchToolBlob` | [persistence](docs/subsystems/persistence.md), [subagents](docs/subsystems/subagents.md) |
| Usage | `WatchChatUsage`, `UsageStats` | [persistence](docs/subsystems/persistence.md) |
| Tools and integrations settings | `GetMcpSettings`, `GetWebSearchSettings` | [toolset-mechanics](docs/subsystems/toolset-mechanics.md) |
| Title and goal settings | `GetTitleSettings`, `GetGoalSettings` | [conversation-policy](docs/subsystems/conversation-policy.md), [goals-routines](docs/subsystems/goals-routines.md) |

Deliberately unserved (the UI renders them; the engine answers `UnknownMethod` by design): the collaboration and account surface — sign-in, orgs, repos, clone, uploads, sync — and worktree lifecycle beyond creation-on-demand. See shell-registry.

## Boot path

`main` → `holt_ui::run_app` → `AppState::bootstrap` → `EngineHandle::bootstrap` assembles the engine (device id and instance lock under `~/.holt`; `HOLT_DATA_DIR` overrides) and connects a memory `RpcClient`. The boot gate resolves immediately — local scope, ready connection, no sign-in wall. Per-feature startup policy (which files fail loudly, which adopt legacy rows, which reconcile) lives in the owning pages.

## Data on disk

Plain JSON/JSONL under the data dir; each record written by the engine, atomically (tmp + rename) where a whole file is replaced, and read tolerantly rather than fatally. Read and write policies per file live in the owning page:

- `chats.json`, `spaces.json` — shell-registry
- `transcripts/<chatId>.jsonl` (+ legacy snapshots), `history/<chatId>.jsonl`, `usage/<chatId>.jsonl`, `usage/archive.jsonl` — persistence
- `queues/<chatId>.json` — conversation-policy
- `provider-credentials.json`, `provider-store.json`, `provider-settings.json`, `provider-logos/` — provider-transport
- `provider-mode/<chatId>.json` — modes
- `turn-changes/<chatId>/<messageId>.json` — workspace-git
- `subagents/<parentChatId>/` — subagents
- `images/` — workspace-files
- `mcp.json`, `web-search.json`, `search-backends.json` (user-written), `title-settings.json`, `goal-settings.json`, `permission-mode-default.json`, `routines.json` — their owning pages
- `device-id` (plain text), `engine.lock`, `logs/` — shell-registry

## Provenance

- gpui is vendored under `third_party/gpui/` — a frozen snapshot of a Zed fork carrying the glass/edge-fade patches the UI depends on. Edit it in place; never resolve gpui from git. [docs/research/gpui.md](docs/research/gpui.md) and the vendored sources are the API truth.
- `LICENSE` is GPL-3.0; the exception is `crates/core`, which keeps the MIT license of the pi packages it was ported from.
