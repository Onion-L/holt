# Holt — Architecture

A desktop-only UI shell. The frontend talks to a small in-process engine; its
first real capability is the `pi-core-rs` agent loop. See README.md for what
was removed.

## Topology

```
gpui UI ── in-memory RPC (ndjson envelopes) ── LocalEngine + pi-core agent loop
```

One binary, headed only. The UI never links backend logic directly: it talks
the typed RPC contract in `crates/rpc` over an in-process duplex
(`holt_rpc::memory_client`). The local backend implements the `RpcService`
contract; another backend can slot in behind the same trait.

## Crates

| Crate | Role |
| --- | --- |
| `apps/holt` | The binary: logging setup + `holt_ui::run_app`. No CLI. |
| `crates/ui` | The whole gpui viewport (~69k lines): shell, sidebar, transcript, composer, terminal/diff panes, settings, themes. Agent-agnostic — it renders `MessagePart`s from `holt-doc`, never raw agent events. |
| `crates/engine` | The backend adapter. `LocalEngine` serves the current in-memory chat/session/transcript runtime, discovers built-in providers and models through `pi-core-rs`, owns credential persistence and the title-task settings record (ADR-0012) plus the one-shot Title task in its `title_task` module, runs `pi-core-rs::agent_loop`, and serves the git capability (branches, checkout diffs, history, fetch) on git2 — all git2 access confined to its `git` module — plus the skills catalog (ADR-0005/0006) in its `skills` module, workspace path search (`SearchFiles`) in its `path_search` module, the per-chat History record and Compaction (ADR-0010/0011) in its `history`/`compaction` modules, and a test-only scripted-provider seam (`EngineConfig::stream_fn`). Unsupported surfaces (terminals, worktrees, change requests, uploads) still return empty watches or unknown-method replies. |
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
- Title settings (ADR-0012): `GetTitleSettings` / `SaveTitleSettings` — the
  engine-owned title-task record (`TitleSettings` in `title-settings.json`),
  both replying `TitleSettingsState` (settings + validation warning).
  Saving rejects unresolvable provider-qualified models and empty or
  out-of-bounds instructions; missing credentials are a warning, never an
  error.
- Permission modes (ADR-0014): a chat's mode rides its `ChatConfig`
  (`permissionMode`, kebab-case tiers; stored sandbox-era values remap on
  read). `Mutate setChatPermissionMode` (`{chatId, mode}`) switches a chat —
  the stored mode is authoritative; a Turn snapshots it at start, so a
  switch lands from the next Turn — and records the device's sticky
  default for new chats (`permission-mode-default.json`, the title-settings
  pattern; first launch defaults to confirm-changes). In confirm-changes
  every mutating call (write/edit/bash) pauses the Turn behind a pending
  Approval — a gate chip on the call's Tool part in the transcript watch —
  until `ResolveApproval` (`{approvalId, verdict}`: allow / always-allow /
  deny with a note) answers or interrupt cancels it; denials settle as
  error tool results the model reads while the Turn continues, and
  interrupted or restarted-mid-approval gates settle as aborted. An
  always-allow records a chat-scoped, in-memory session grant (bash by
  command prefix, write/edit by exact resolved path) checked before the
  gatekeeper — it holds across mode switches and never persists. In
  auto-review each mutating call is first judged by one extra model pass
  through the same transport the run uses (the chat's own model, no
  separately-configured reviewer): a pass executes, a rejection blocks
  with the reviewer's reason, an unclear or failed review rejects
  closed, and no Approval is created. Reads, grep, and full-access
  never gate. The gate rides the agent loop's `before_tool_call` hook
  (no upstream changes); the Title task and Compaction mount no tools
  and never see it.
- Catalog: provider-scoped `ListModels`, plus `ListCommands` and `ListSkills`
  (the skills catalog, ADR-0005/0006: one fresh scan of the chat's three
  skill roots — project `.agents/skills` at the cwd, personal
  `~/.agents/skills`, holt `<data_dir>/skills` — returning invocable
  entries with source root, shadowed entries, and load diagnostics).
- Path search: `SearchFiles` (`{query, chatId|spaceId}`) fuzzy-matches files
  and folders under the chat's cwd (or the space's path before a chat
  exists) for the composer's `@` popup — hidden and ignored entries are
  included, `.git` is excluded. This is deliberately broader than the
  agent's content search, which keeps its ignore-respecting traversal.
  Path references themselves are not an RPC surface: picker, drag, and `@`
  selections travel as plain prompt text (an appended absolute-path list,
  or inline quoted absolute paths for `@`) through the ordinary queue —
  nothing is uploaded, copied, snapshotted, or eagerly read.
- Transcript: `WatchDocMessages` (`TranscriptFrame` stream per chat).
- Message queue: `WatchMessageQueue` (`MessageQueue` snapshots
  per chat) and `ContinueMessageQueue` (`{chatId}`, replies with a snapshot).
  `QueueCommand run`/`invokeSkill`/`compact` acknowledge durable acceptance
  by `messageId`; the same identity is deduplicated across pending, started,
  and completed work, including after restart. Pending items are typed
  (`PendingKind`): an ordinary message or a skill invocation starts its own
  Turn, a manual Compaction shares the channel without one (ADR-0011).
  `EditQueuedMessage` (`{chatId, messageId, prompt}`) rewrites only the one
  editable field — an ordinary message's body or a skill invocation's extra
  instructions; identity, position, kind, and the captured model settings
  are the queue's, and a pending Compaction has no editable field — and
  `DeleteQueuedMessage` (`{chatId, messageId}`) removes one, preserving the
  order of the rest. Both acknowledge only after the queue file is durably
  replaced and reply with the accepted snapshot; a started item refuses both
  instead of touching the active Turn. Admission errors arrive through the
  Watch.
- Mutations: `Mutate` (createChat/createSpace/…), `QueueCommand`.
- Git capability (ADR-0001/0002, all served on the git2 backend inside
  `engine::git`): `ListRefs` / `ListBranches` (default-first local
  branches), `SwitchRef` / `CreateBranch` (safe checkouts), the checkout
  diff family — `WatchCheckoutDiffs` (live, debounced fs-notify frames
  keyed by the space's canonical checkout identity
  `sha256(deviceId ‖ NUL ‖ git_dir)`), `GetCheckoutDiff` /
  `GetCheckoutFileDiffText` in working-tree / branch (merge-base) / commit
  / turn (net-change baseline, ADR-0003) modes — plus `ListGitHistory`
  (paged topo-ordered graph) and `FetchAll` (prune, system credentials,
  30 s timeout).
- Capability surfaces the UI keeps rendered but the local backend leaves empty:
  terminals, worktrees, change requests, and uploads.

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

Skills (ADR-0005/0006) ride the catalog above: every run appends a
metadata-only `<available_skills>` block to the system prompt from a
fresh three-root scan (the upstream loader and formatters; budget-capped),
the model self-serves `SKILL.md` through the mounted read tool, and the
composer's `/skill` slash command queues a typed `InvokeSkill` item whose
prompt the engine formats from the skill's content — resolved against a
fresh catalog when the queue admits the item, never frozen at submission —
the raw directive never reaches the model, and both invocations and
`SKILL.md` reads render as compact chips in the transcript.

The implemented agent slice is intentionally narrow: provider configuration,
provider/model discovery, `createChat`/`renameChat`, chat/session watches, `QueueCommand`
run/interrupt/`invokeSkill`/`compact`, and streamed transcript frames. The run loop mounts pi-core's
built-in read/write/edit/bash tools (via `engine::tools`, a local
`ExecutionEnv` rooted at the chat's cwd) plus holt's own content-search tool,
named `grep` (ripgrep's crates in process, ADR-0004); the transcript folds their
calls and results into `MessagePart::Tool` chips.

Conversation memory (ADR-0010/0011): a chat's **History** — the model-facing
`AgentMessage` sequence — persists as its own append-only JSONL record next to
the Transcript (`engine::history`), appended per message as a Turn runs and
replayed on load, so the model remembers exactly what the Transcript shows
across restarts and crashes. A repair invariant keeps the record a valid
provider request (interrupted Turns keep an honest interrupted record with
synthetic error tool results; errored Turns keep the prompt and drop the
failed answer), and a damaged file is quarantined (`.corrupt`) with a
Transcript notice rather than blocking the chat. **Compaction**
(`engine::compaction`) shrinks a long History into a model-written summary
plus a verbatim recent tail, using pi-core's compaction primitives through
the same stream function the agent loop uses: automatically before a Turn
and between tool rounds (`prepare_next_turn`), manually with the `/compact`
slash command (a typed queue item — the `Compacting` session status is
interruptible like a run when the queue admits it), and unconditionally on
the Turn after a context overflow. The Transcript never shrinks — dividers
(expandable, with before/after token counts and the trigger) and notices
mark what happened. Queue items — ordinary messages, skill invocations,
and manual Compaction commands — persist in `queues/<chatId>.json`,
independently of Transcript and History. Each chat has one FIFO consumer
and one execution lock held through preparation, execution, History
repair, Transcript settlement, and queue completion. An interrupt requests
cancellation and pauses the queue; the channel stays occupied until
cleanup completes. Approval waits and automatic/manual Compaction share
that boundary. Successful work advances automatically — a completed
Compaction included. Execution failures pause remaining work; admission
failures (missing credentials or an unresolved skill) retain the head with
an error without creating a Turn. Nothing to compact completes the command
with a Transcript notice and advances the queue without a model request.
New submissions preserve pause state; Continue resumes it. Deleting the last
pending item also clears pause when no item is executing and there is no
queue-level error, so the next submission runs normally.

The pending-to-started checkpoint is atomically replaced and synced before
any model or tool work. Restart repairs a started Turn as interrupted,
never requeues it, settles a started Compaction with no record at all (it
was never a Turn), and restores unstarted work paused. The queue keeps
accepted message identities to make delivery retries idempotent. A failed
conversation write pauses consumption and requires storage repair and an
app reopen before further admission; an unreadable queue is retained and
blocked instead of overwritten. A completion status is published only
after queue completion is recorded or its persistence failure is surfaced.

The composer watches only its selected chat's queue, above the input;
pending items are absent from Transcript and History. An empty paused queue
is hidden unless it has a queue-level error. Model and reasoning
are captured on submission. Permission mode and live checkout identity are
read at Turn admission, when the latest-Turn diff baseline is refreshed.
The queue is consumed by the engine even when its chat is not selected.
Rows render their command kind — an ordinary body, `/skill <name>` with
its extra instructions, `/compact` — and offer the actions the kind
allows: Run now, edit, and delete for ordinary messages and skill
invocations (a skill edits only its extra instructions), delete alone for
a pending Compaction. The editor is its own card (the composer's draft
text is unrelated), Escape cancels, and a failed save keeps the unsaved
text with the error.

Queue mutations serialize with execution admission on the queue lock. The
admission checkpoint (pending-to-started) re-reads the item from the queue,
so an edit that lands between the consumer's pick and the checkpoint wins;
a delete that lands there removes the item and the consumer simply moves
on — the refused admission never pauses the queue or stamps an error on a
different item. After the checkpoint the item is execution's property:
mutations fail with an already-executing error and the run proceeds exactly
once.

Steer (ADR-0015) is the explicit priority action: it interrupts active
work, waits for its cleanup, and starts the selected message as a new Turn
ahead of ordinary pending work — pending Steer requests keep their
submission order, and Run now on a paused queue authorizes only the
selected item. Skill invocations support the same Run now/Steer promotion;
a pending Compaction executes strictly in order. `/skill` and `/compact`
join the same queue as ordinary messages (ticket 04); `/compact` remains
outside the Turn model. Terminals, worktrees, change requests, and uploads
remain unserved.

The engine's integration tests drive whole Turns through `RpcService::handle`
against a scripted provider injected via `EngineConfig::stream_fn` (set only
by tests): the fake transport records every request's message list — what
the model would receive — and replies from a script (text, tool calls,
aborted streams, provider errors, pinned usage), so persistence, repair,
compaction, and overflow behavior are asserted without a real provider
(`crates/engine/tests`).

## Provenance notes

- gpui is vendored under `vendor/gpui/` (25 crates + `tooling/perf`, its own
  trimmed workspace root; `vendor/` additionally holds the xim-rs / font-kit /
  scap sources gpui's platform backends need) — a snapshot of a zed fork
  carrying the glass/edge-fade patches the UI depends on. It is a frozen
  asset: edit it in place when needed; no dependency resolves from git.
- `THIRD_PARTY_NOTICES.md` carries upstream attribution obligations.
- Historical design docs for removed subsystems (sync, agent drivers, edge)
  were deleted with them; `docs/` keeps UI/theme/gpui/memory references.
