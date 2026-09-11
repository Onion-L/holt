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
| `crates/engine` | The backend adapter. `LocalEngine` serves the current in-memory chat/session/transcript runtime, discovers built-in providers and models through `pi-core-rs`, owns credential persistence and the title-task settings record (ADR-0012) plus the one-shot Title task in its `title_task` module, runs `pi-core-rs::agent_loop`, and serves the git capability (branches, checkout diffs, history, fetch) on git2 — all git2 access confined to its `git` module — plus the skills catalog (ADR-0005/0006) in its `skills` module, workspace path search (`SearchFiles`) in its `path_search` module, the per-chat History record and Compaction (ADR-0010/0011) in its `history`/`compaction` modules, the Turn change-set baseline, frozen result, and durable per-Turn history (ADR-0024) in its `turn_changes`/`turn_change_watch`/`turn_change_store` modules, and a test-only scripted-provider seam (`EngineConfig::stream_fn`). Unsupported surfaces (worktrees, change requests, uploads) still return empty watches or unknown-method replies. |
| `crates/rpc` | The typed control plane: framing, `RpcClient` (call/subscribe), `RpcService` dispatch, memory transport. Method names live in `rpc::methods` — that module is the full UI↔backend contract. |
| `crates/proto` | Shared types: `ProviderId`, provider-qualified models and run configuration, entities (Chat/Space/Device/Session), `EngineInfo`, and view derivations. |
| `crates/doc` | Loro-CRDT session docs and the `MessagePart`/`TranscriptFrame` types the transcript renders. |
| `crates/theme`, `crates/syntax` | Theme library and syntax highlighting. |

## The RPC contract (what a real backend must serve)

Defined by `crates/rpc/src/lib.rs::methods` and consumed by
`crates/ui/src/state.rs` (`attach_engine` starts the standing watches):

- Identity/barrier: `EngineInfo`, `EngineReady`.
- Entity watches: `WatchChats`, `WatchSpaces`, `WatchSessions`
  (each emits `Vec<T>` snapshots), `WatchConnectivity`.
- Provider configuration: `ListProviders`, `SaveProviderKey`,
  `RevealProviderKey`, `RemoveProviderKey`, `AddProviderModel`.
- Title settings (ADR-0012): `GetTitleSettings` / `SaveTitleSettings` — the
  engine-owned title-task record (`TitleSettings` in `title-settings.json`),
  both replying `TitleSettingsState` (settings + validation warning).
  Saving rejects unresolvable provider-qualified models and empty or
  out-of-bounds instructions; missing credentials are a warning, never an
  error.
- Web search settings (ADR-0023): `GetWebSearchSettings` /
  `SaveWebSearchSettings` (`{backend, apiKey}`, both replying the masked
  `WebSearchSettingsState` plus the picker's launch options — Zhipu,
  Bocha, Brave), `RevealWebSearchKey`, and `RemoveWebSearchSettings` —
  the user-chosen search-backend record in `web-search.json` under the
  credentials pattern (0600, atomic replace, malformed fails startup
  loudly). Saving validates the backend id against the offered list and
  a non-empty key; the key is an independent record, never shared with a
  same-vendor provider key. The engine resolves the configured backend
  once per Turn admission through the built-in adapter table — Zhipu,
  Bocha, and Brave, one adapter module each over a shared transport
  scaffolding (whole-exchange budget, cancellation race); request and
  response shapes and error mapping stay per adapter — a mid-Turn
  change lands from the next Turn, and an unconfigured backend leaves
  the `web_search` agent tool unmounted: absent, never erroring.
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
  closed, and no Approval is created. Reads, grep, the web tools, and
  full-access never gate. The gate rides the agent loop's
  `before_tool_call` hook (no upstream changes); the Title task and
  Compaction mount no tools and never see it.
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
  existing sources are not copied or snapshotted. Image previews may read
  pixels locally; submitting the reference still sends only a path.
- File sidebar (ADR-0020): `ListWorkspaceEntries`
  (`{chatId|spaceId, path?}`, one directory level — dirs first, hidden
  entries in, `.git` out, symlink kinds resolved against the root,
  `truncated` at the per-directory cap) and `ReadWorkspaceFile`
  (`{chatId|spaceId, path}` — editable UTF-8 up to 2 MiB plus BOM /
  line-ending facts and an opaque disk `version` token, or a typed
  `unsupportedReason` for oversized, non-UTF-8, and binary files) and
  `ReadWorkspaceImage` (`{chatId|spaceId, path}` — the bounded sniffed
  image read for the sidebar's image tabs, fenced behind the same root
  containment, `.git` exclusion, and symlink-landing rules as every other
  workspace read even though the general `ReadImage` reads any local path;
  replies `WorkspaceImageData` with the canonical resolved path) and
  `SaveWorkspaceFile` (`{chatId|spaceId, path, text, version, bom}` —
  version-checked atomic write preserving permissions; a moved disk version
  is a `versionConflict` REPLY, never an overwrite, and a missing file is
  never silently recreated), `WriteWorkspaceFileAs` (create-new destination
  inside the root; collisions refuse, originals stay untouched), and
  `WatchWorkspaceEntries` (coalesced ~200ms-quiet frames of changed paths
  under the root — live tree refresh, clean-editor reload, and dirty-editor
  conflict states; a confirmed overwrite rides `SaveWorkspaceFile`'s
  `expectDiskVersion` against the reviewed token), plus the entry
  management group `CreateWorkspaceEntry` (`{chatId|spaceId, parentPath?,
  name, isDir}` — existing in-root parents only, atomic no-clobber creation),
  `RenameWorkspaceEntry` (`{chatId|spaceId, path, newName}` — in-place
  sibling rename acting on the ENTRY itself: a symlink renames as a link,
  never as its target; noreplace semantics where the platform offers them,
  case-only renames pass), `MoveWorkspaceEntry`
  (`{chatId|spaceId, path, destinationDirectory}` — cut/paste into an
  existing in-root directory, keeping the name; the engine revalidates both
  ends and refuses outside-root, `.git`, collisions, pastes into the
  entry's own subtree, and same-directory no-ops — one rename, never a
  copy/delete pair, so a cross-device move fails instead of degrading), and
  `TrashWorkspaceEntry` (`{chatId|spaceId, path}` — the OS trash via
  `NSFileManager trashItemAtURL` on macOS, entry-level like rename; a
  trash that is unavailable or refuses fails and nothing is ever
  permanently deleted as a fallback). These serve the far-right file tree
  and its contents tabs. Root containment, `.git`
  exclusion, and symlink fences are enforced engine-side in
  `engine::files` on every request; the UI never touches the workspace
  filesystem itself.
- Local images: `StageImage` (`{data}` base64) validates and durably saves
  pasted pixels under `<data_dir>/images/<uuid>.<ext>`, returning a stable
  `ManagedImage` path. `ReadImage` (`{path}`) returns validated local image
  bytes for UI preview; `ReleaseImage` (`{path}`) only removes Holt-managed
  files without retained references. Payloads and replies are typed in
  `crates/rpc/src/images.rs`. These methods are separate from unserved uploads.
- Transcript: `WatchDocMessages` (`TranscriptFrame` stream per chat or
  Subagent document). `FetchToolBlob` serves completed Subagent transcripts
  as JSON text for a `{parentChatId}/{subagentDocId}` reference.
- Terminals (ADR-0017): `OpenTerminal`, `WriteTerminal`, `ResizeTerminal`,
  `SubscribeTerminal`, and `CloseTerminal` serve user-operated PTYs in
  `engine::terminals`, rooted at the owning Chat's working directory.
  Chat-less terminals (the new-chat canvas, the no-project empty state)
  carry an explicit `cwd` — the selected Space's path — and fall back to
  the user's home directory when neither exists.
  `ListTerminals` supplies running status for close confirmation;
  `CloseAllTerminals` releases sessions when the window closes. The default
  login shell inherits the user's environment and shell configuration.
  Output is base64 with monotonic sequence numbers and a bounded 1 MiB /
  4,096-event replay buffer. Evicted replay emits an explicit `Gap`; the UI
  reports incomplete output and keeps the same process. Terminal viewports
  use Alacritty with 10,000 scrollback lines. Tabs group independent split
  panes, retained while moving between the bottom drawer and right pane.
  Closing running terminals requires confirmation, including Chat deletion,
  native window closure, and macOS Dock quit. The vendored GPUI macOS backend
  exposes `on_should_quit` so native quit can await that decision.
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
- Turn terminal events (ADR-0019): `WatchTurnTerminalEvents` emits one typed
  `TurnTerminalEvent` (`holt_rpc::turns` — `eventId`, `chatId`, `messageId`,
  `outcome` of `succeeded` / `failed` / `interrupted`, `finishedAt`, plus an
  internal-only failure reason consumers never display, and — once the
  Turn's change-set record is durable — its final `changeSet`, ADR-0024,
  where absence means unavailable, never "no changes") per real main-chat
  Turn, published only after the Turn's Transcript, History, and queue
  completion are durably settled; a completion that cannot be persisted
  keeps the existing queue/session error and emits nothing. The stream is
  live-only — no synthetic initial event, no persistence or replay across
  restart — and publishing is fire-and-forget, so a closed or lagging
  consumer can never fail a Turn or delay the queue. Subagents, manual
  Compaction, Title tasks, and admission failures never appear here. The UI
  consumer is one application-scoped notification controller
  (`ui::notifications`): created during bootstrap, independent of any
  window, it dedups by `eventId` and posts device-local OS banners
  (Chat title + outcome only) when no Holt window is active. Banners are
  tagged by Chat id; a click retracts the banner, activates Holt (reopening
  the main window when none is open), and selects the Chat when it still
  exists, and marking a Chat seen retracts its banner — all best-effort.
- Mutations: `Mutate` (createChat/createSpace/…), `QueueCommand`.
- Git capability (ADR-0001/0002, all served on the git2 backend inside
  `engine::git`): `ListRefs` / `ListBranches` (default-first local
  branches), `SwitchRef` / `CreateBranch` (safe checkouts), the checkout
  diff family — `WatchCheckoutDiffs` (live, debounced fs-notify frames
  keyed by the space's canonical checkout identity
  `sha256(deviceId ‖ NUL ‖ git_dir)`), `GetCheckoutDiff` /
  `GetCheckoutFileDiffText` in working-tree / branch (merge-base) / commit
  / turn (net-change baseline, ADR-0003) modes — plus `ListGitHistory`
  (paged topo-ordered graph), `FetchAll` (prune, system credentials,
  30 s timeout), and the Git panel's write trio `StagePaths` /
  `UnstagePaths` / `CommitStaged` (ADR-0022: path validation, conflicted
  paths, and mid-merge/rebase/revert/cherry-pick states all refuse
  engine-side; commit identity is the repo's git config, author ==
  committer; the agent tool surface stays read-only).
- Turn change sets (ADR-0024): `GetTurnChangeSet` (`{chatId}`, plus
  `messageId` to address one specific Turn) and the `WatchTurnChangeSet`
  stream (`{chatId}`) serve the current main-chat Turn's net Git change from
  its admission baseline to the live or final working tree. Both reply
  `holt_proto::TurnChangeSetReply` — an explicit `unsupported` for a non-Git
  working directory, never an empty change set, otherwise Git-derived file
  status, line counts, and the `live` / `final` phase. Subagent edits ride
  the parent Turn's set (a child shares the parent's working directory). The
  engine owns the in-memory baseline and frozen result
  (`engine::turn_changes`); `engine::turn_change_watch` is the debounced
  live stream whose settled frame is driven by the ADR-0019 Turn terminal
  event. Settled Turns persist durably (`engine::turn_change_store`): at
  settlement — after queue completion, before the terminal event publishes —
  the frozen summary plus the immutable per-file before/after content
  (captured together in one locked Git pass, so summary and content can
  never disagree; each side capped at 1 MiB; a rename's old side reads its
  pre-move path; binaries keep hashes, never text) is written atomically,
  once per Turn, to `turn-changes/<chatId>/<messageId>.json`. A `messageId`
  read answers from memory first and then the record, which survives
  restarts, later workspace edits, and a working tree that can no longer be
  captured (only a Turn with no record anywhere answers by its working
  directory); a malformed record degrades to "no record" without blocking
  the chat.
  `GetCheckoutFileDiffText` in `turn` mode takes the same `messageId`: a
  settled Turn serves its immutable stored pair (`stale` always false),
  while the live current Turn still reads the working tree against its
  baseline. A persistence failure never fails the Turn or its terminal
  event — the event then simply carries no `changeSet` — and deleting a
  chat reclaims its whole change-set history.
  The UI consumer is the transcript's per-Turn change card (ticket 03):
  `AppState` keeps the selected chat's sets keyed by Turn id — the watch's
  live frames while the Turn runs, its final frame at settle (immutable in
  the store; failed and interrupted Turns keep theirs), and history
  restored by id when the transcript's opening reset lands — and the
  transcript appends one card row after each Turn's last entry. Empty sets
  (including a net-zero settle retiring a live card) and non-Git
  workspaces render no card; binary entries show status only with no line
  counts; the Review/Open actions are ticket 04.
- Capability surfaces the UI keeps rendered but the local backend leaves empty:
  worktrees, change requests, and uploads.

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
named `grep` (ripgrep's crates in process, ADR-0004), the Workspace-aware
`read_chat` tool for another Chat's user-visible Transcript (ADR-0018), and the
two web tools (ADR-0023): `web_fetch` retrieves one http(s) URL and returns its
full converted text, bounded but never summarized, while `web_search` queries
the user-configured backend — resolved once per Turn admission, absent from
the toolset (not erroring) when none is configured. Neither enters the
ADR-0014 gate: fetching reads a page the way `read` reads a file. The
transcript folds their calls and results into `MessagePart::Tool` chips.
Parent runs also mount the foreground `Agent` delegation tool (ADR-0016).

## Subagents

`engine::subagents` owns foreground delegation through the existing pi-core-rs
loop, without upstream changes. The fixed Explorer and Worker roles inherit
the parent Turn's model, reasoning, working directory, and Permission mode.
Explorers keep only the read-only set — `read`, `grep`, `read_chat`,
`web_fetch`, and the configured `web_search`; Workers mount the full toolset,
write/edit/bash included. Neither can delegate. Each child starts with
independent History, a Task brief, applicable ancestor AGENTS.md instructions,
and a fresh skills listing. Workers share the actual working directory; the
parent assigns file ownership, without automatic worktrees, merging, or
rollback.

An engine-wide semaphore allows four running children; a parent Turn may
create eight in total. There is no fixed child request-count or total-runtime
cap. Child cancellation descends from the parent Turn; Stop and Steer wait for
child cleanup before the next Turn. A child failure returns an error and
partial output without cancelling its siblings. Children use the existing
Compaction implementation against their own History; context overflow fails
the child without retry or changing models.

Child documents use `{parentChatId}--sub--{uuid}` identities and independent
transcript/History files under `<data_dir>/subagents/{parentChatId}`. They reuse
the transcript runtime and event folding, but never join the chat registry,
session list, or queue consumer. Their queue is unused and never persisted.
Child records survive restart for viewing; running records recover as aborted,
with failed parent spawn chips, and execution is never resumed. Deleting the
parent also deletes its child records. Completed children leave the active
registry and can be loaded from disk for inspection.

Spawn chips carry the child document reference, live tail, and completion
status. Child Approvals also appear in the parent Transcript with a typed
`ToolGate.origin` that opens the child tab; these are display-only projections,
not parent History messages. Always-allow grants are shared with the parent
chat. Child results carry usage, including child Compaction and auto-review
requests, in the parent's tool-result History record.

Only the final summary enters parent History, bounded to 12,000 tokens measured
with the shared o200k tokenizer (a stable output budget, not a claim about a
provider's billing tokenizer). The full final output is saved under the child's
`results/` directory; truncation includes an explicit notice and an absolute
file path usable by `read`. The independent Transcript retains intermediate
work. `FetchToolBlob` only serves finished child transcripts; live documents
use `WatchDocMessages`.

## Image capability

The composer and Transcript share the local image viewer. Existing picker,
drop, and inline image references remain live paths; pasted screenshots are
engine-owned Managed images. Neither previewing nor submitting a path adds
pixels to a model request. Only the existing `read` tool introduces image
content, through its bounded reader and image processor in `engine::tools`.
The actual image tool result persists in History; Transcript rendering
remains agent-agnostic and does not need a new image MessagePart. Compaction
keeps images in its retained tail and summarizes older content as text.

PNG, JPEG, static WebP, and the first frame of GIF/animated WebP are supported.
APNG and other formats are not visual-read formats. Decoding applies image
orientation, caps input at 25 MiB and decoded pixels at 32 * 1024 * 1024,
and sets a 256 MiB codec allocation ceiling. The UI preserves source detail
within these limits. Model input becomes PNG with a maximum edge of 2048
pixels and maximum size of 5 MiB, proportionally reduced as necessary;
tool text records original/output dimensions and first-frame conversion.
Source files are never rewritten. Decode failures are unsuccessful tool
results. Two engine processing jobs, four thumbnail loads, serialized UI
decoding, and a 64 MiB / 256-entry thumbnail cache bound background work.
The viewer invalidates changed/deleted files and ignores stale completions.

`ModelInfo.imageCapability` is `supported`, `unsupported`, or `unknown`.
Built-ins derive it from the provider catalog; custom models stay unknown.
Known nonvisual models show a composer limitation while accepting paths;
image reads report lack of vision, and text reads keep working. Unknown
custom models may attempt real image requests through their provider's
transport. Rejection follows normal Turn/queue failure handling; no automatic
model switch or text-only retry occurs. Capability belongs to the execution
model captured by the accepted queue item.

Managed files never move during draft/chat transitions. Explicit draft
removal protects other drafts and send retries before release; the engine
also checks durable queues, Transcript, and History. Startup reconciles
durable references before serving image RPCs, reclaiming abandoned images
and images from deleted chats only when no retained reference needs them.
Unreadable or malformed durable records conservatively retain managed files.
External files are never cleanup candidates. Drafts themselves are not
persisted by this feature.

## Conversation lifecycle

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
New submissions preserve pause state, with one exception (ADR-0021): an
attended send — a first acceptance arriving while the execution channel is
settled and the queue is paused — is admitted at once through a one-shot,
persisted grant bound to its message id; it runs alone ahead of parked
work, and the queue returns to its pause when it settles. Continue resumes
the queue and lifts the single-run scope of an outstanding grant. Deleting
the last pending item also clears pause when no item is executing and there is no
queue-level error, and a queue that settles empty with no queue-level error
comes back clean on reload as well, so the next submission runs normally.

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
outside the Turn model. Worktrees, change requests, and uploads
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
