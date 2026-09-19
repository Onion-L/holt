# Holt — Architecture

A desktop-only UI shell. The frontend talks to a small in-process engine; its
first real capability is the `pi-core-rs` agent loop. README.md covers the
project overview and quick start.

## Topology

```
gpui UI ── in-memory RPC (ndjson envelopes) ── LocalEngine + pi-core agent loop
```

One binary, headed only. The UI links `crates/engine` for exactly one thing —
assembling the local backend at bootstrap (`EngineHandle::bootstrap` in
`ui/state.rs`); no feature code in `crates/ui` calls backend logic — it talks
the typed RPC contract in `crates/rpc` over an in-process duplex
(`holt_rpc::memory_client`). The local backend implements the `RpcService`
contract; another backend can slot in behind the same trait.

## Crates

| Crate | Role |
| --- | --- |
| `apps/holt` | The binary: logging setup + `holt_ui::run_app`. No CLI. |
| `crates/ui` | The whole gpui viewport (~97k lines): shell, sidebar, transcript, composer, terminal/diff panes, settings, themes. Agent-agnostic — it renders `MessagePart`s from `holt-doc`, never raw agent events. |
| `crates/engine` | The backend adapter. `LocalEngine` serves the current in-memory chat/session/transcript runtime, discovers providers and models through `pi-core-rs` overlaid with the user's `provider-store.json` (one merged catalog built at boot) topped by the live settings layer in `provider-settings.json` (model records, custom providers, hidden ids — ADR-0028, read per call), owns credential persistence and the title-task settings record (ADR-0012) plus the one-shot Title task in its `title_task` module, runs `pi-core-rs::agent_loop`, and serves the git capability (branches, checkout diffs, history, fetch) on git2 — all git2 access confined to its `git` module — plus the skills catalog (ADR-0005/0006) in its `skills` module, workspace path search (`SearchFiles`) in its `path_search` module, the per-chat History record and Compaction (ADR-0010/0011) in its `history`/`compaction` modules, the per-chat usage ledger in its `usage` module and the device-level usage aggregate behind `UsageStats` in its `usage_stats` module, the Turn change-set baseline, frozen result, and durable per-Turn history (ADR-0024) in its `turn_changes`/`turn_change_watch`/`turn_change_store` modules, and a test-only scripted-provider seam (`EngineConfig::stream_fn`). Unsupported surfaces (worktrees, change requests, uploads, sync/account) still return empty watches, static stubs, or unknown-method replies. |
| `crates/rpc` | The typed control plane: framing, `RpcClient` (call/subscribe), `RpcService` dispatch, memory transport. Method names live in `rpc::methods` — that module is the full UI↔backend contract. |
| `crates/proto` | Shared types: `ProviderId`, provider-qualified models and run configuration, entities (Chat/Space/Device/Session), `EngineInfo`, view derivations, the per-chat usage frame (`ChatUsage`: ledger totals plus occupancy), and the device-level usage aggregate (`UsageStatsReply`). |
| `crates/doc` | The wire types both ends exchange — `MessagePart`, `SessionMessageEntry`, `TranscriptFrame`, the typed part payloads — plus transcript-frame diffing. Persistence is plain JSON/JSONL owned by `crates/engine` (see "Data on disk" below); the crate's Loro session/workspace schemas and its HLC registry port are dormant — nothing outside `crates/doc` links them. |
| `crates/theme`, `crates/syntax` | Theme library and syntax highlighting. |

## The RPC contract (what a real backend must serve)

Defined by `crates/rpc/src/lib.rs::methods` and consumed by
`crates/ui/src/state.rs` (`attach_engine` starts the standing watches):

- Identity/barrier: `EngineInfo`, `EngineReady`, `LocalDevice` (the local
  device id).
- Static stubs the UI polls defensively: `AuthStatus` (always signed-out) and
  `ProbeSync` (a no-op reply).
- Entity watches: `WatchChats`, `WatchSpaces`, `WatchSessions`
  (each emits `Vec<T>` snapshots), `WatchDevices` (the local machine's device
  row only), `WatchConnectivity` (one static Disabled snapshot — no edge
  transports).
- Provider configuration: `ListProviders`, `SaveProviderKey`,
  `RevealProviderKey`, `RemoveProviderKey`, plus the live catalog writes
  (ADR-0028): `SaveModelRecord` /
  `RemoveModelRecord` (a complete record replaces a same-id entry outright),
  `SaveCustomProvider` / `RemoveCustomProvider` (user-defined providers),
  `SetHiddenModels` (listings only — resolution keeps working),
  `ListHiddenModels` (the greyed ids the Settings page unhides), and
  `ResetProviderCatalog` (per-provider, or global when `providerId` is
  absent). `ListApiDialects` serves the record form's dialect dropdown
  (pi-core's compat registry). Every write takes effect without a restart. The model-setup
  dialog's surface rides the same store: `StartModelSetupChat`
  (starts a fresh session-scoped hidden setup chat, deleting any earlier
  one), `ListModelProposals`,
  `ApplyModelProposal` (the review panel's write button), and
  `DiscardModelProposal` (its discard button).
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
- Jev connection (ADR-0027): `GetJevSettings` / `SaveJevSettings`
  (`{apiKey}`, both replying the masked `JevSettingsState`),
  `RevealJevKey`, and `RemoveJevSettings` — the user's own TypeSafe key
  in `jev.json` under the credentials pattern (0600, atomic replace,
  malformed fails startup loudly). Saving trims and refuses a blank key
  with no network validation; the key is an independent record, never
  shared with a same-vendor provider key. The engine also carries the
  harness-written TypeSafe client (`jev.rs`: one decision endpoint,
  atomic Noul question set, synthesis, retry/timeout policy). No feature
  consumes the connection yet — the `jev-review` permission mode it
  served was removed (ADR-0026 → 0027); future Jev-powered features
  mount from here.
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
  full-access never gate. One invariant carries from ADR-0029/0030: should
  an apply-shaped tool ever be mounted again, `model_apply`'s name always
  meets the human gatekeeper — full-access included, no session grant
  passes it, and auto-review never substitutes — because a catalog write
  steers where the API key is sent. The gate rides the agent loop's
  `before_tool_call` hook (no upstream changes); the Title task and
  Compaction mount no tools and never see it.
- Plan Mode (ADR-0025): a chat-level planning checkpoint orthogonal to the
  permission mode, carried on the chat row (`planMode`: just the
  permission mode recorded on entry — restored on plan approval, never
  moved by the entry itself). `EnterPlanMode` / `ExitPlanMode`
  (`{chatId}`; both idempotent) and `GetPlanMode` reply the
  `PlanModeState` view (`active`, the entry mode). A planning Turn runs
  the read-only exploration toolset with a prompt that makes a complete
  `<proposed_plan>` Markdown block in the assistant's ordinary text the
  only submission channel; the transcript folds each block into an
  approval card. `ResolvePlanApproval` (`{chatId, verdict, feedback?}`,
  verdict `approve | reject | remain`, requires a planning chat with a
  pending card) applies the verdict: approve exits Plan Mode restoring
  the entry mode and enqueues an approval follow-up prompt as an
  ordinary run — the plan is already in the conversation History, so
  the implementation Turn carries it naturally and starts on its own;
  reject keeps planning and a non-empty feedback is enqueued as the
  revision loop's next planning input; remain changes nothing but the
  cards. Exiting settles
  pending cards as dismissed; switches during a running Turn take effect
  from the next Turn; restart restores the state without auto-starting a
  Turn. (2026-09-12: simplified from plan documents on disk +
  write/submit tools + injection to the conversational `<proposed_plan>`
  convention after reviewing Codex's plan mode; the enforced read-only
  toolset and the approval cards stay.)
- Catalog: provider-scoped `ListModels`, plus `ListCommands` and `ListSkills`
  (the skills catalog, ADR-0005/0006: one fresh scan of the chat's three
  skill roots — project `.agents/skills` at the cwd, personal
  `~/.agents/skills`, holt `<data_dir>/skills` — returning invocable
  entries with source root, shadowed entries, and load diagnostics).
  `ListModels` rows carry `contextWindow`: builtin rows and live model
  records report the catalog's real window, bare custom-id rows null (the
  only window the engine holds for them is the cloned template's guess),
  and clients degrade to absolute
  token counts when it is missing.
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
  `truncated` at the per-directory cap), `WatchWorkspaceGitStatus` (the Git
  panel's live status stream for the same root) and `ReadWorkspaceFile`
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
  permanently deleted as a fallback). The add-space palette browses the
  local machine through `ListFolders` (one directory level) and `ListDrives`.
  These methods serve the far-right file tree and its contents tabs. Root
  containment, `.git` exclusion, and symlink fences are enforced engine-side
  in `engine::files` on every request; the UI never touches the workspace
  filesystem itself. The one fence exception is `ReadWorkspaceFile`: a read
  may also land inside one of the engine's skill roots (personal
  `~/.agents/skills`, holt `<data_dir>/skills`; project skills already sit
  under the cwd) so the transcript's skill chips open a `SKILL.md` in a
  sidebar file tab wherever the skill lives — a read must reach a directory
  under the root (the loader's skill shape), and saves and every other verb
  keep the plain workspace fence.
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
- Token usage: `WatchChatUsage` (`ChatUsage` snapshots per chat, params
  `{chatId}`) reports the chat's whole-ledger gross token total, its
  per-kind breakdown (Turn work, Subagent, Compaction, Auto-review, Title
  task), the record count, the Output speed sums (output tokens and
  generation duration over just the records that carry a measured
  duration — old ledger lines join neither sum), and the context occupancy
  of the request the chat would run next. The occupancy numerator is the
  latest main-run
  provider report — its request input plus both cache fields — and its
  denominator is the context window of the model the queue runs next (the
  executing item, else its pending head), the chat's current selection when
  the queue is empty, and absent for a custom model whose window the engine
  does not know. Occupancy is derived at read time and never persisted: the
  first report flips it from a History-based estimate (`estimated: true`)
  to measured (`estimated: false`).
- Usage overview: `UsageStats` (`{days: 7|30|90}`) answers the device-level
  aggregate in one unary reply (`holt_proto::UsageStatsReply`). It merges
  the live chats' ledgers and the `usage/archive.jsonl` stream — all five
  record kinds, deleted chats included — and returns: the deduplicated
  count of chats with at least one record in range (live ledgers attribute
  by file name, archive rows by their restamped chat id); the six range
  metrics (Input, Output, Cache read, Cache write, Cache hit =
  cache_read / (cache_read + input) with cache writes out of the
  denominator and no rate when no record carried prompt tokens, and Active
  days); per-(provider, model) daily series over exactly the range,
  zero-filled on empty local-timezone days and sorted by total descending
  (the same model name under two providers is two entries); the by-model
  and by-project breakdowns (columns Input | Output | Cache read | Total,
  Total the four token fields summed; a project is the chat's stored
  working directory at full path, so same-named basenames never merge, and
  records with no resolvable directory — deleted chats, cwd-less rows —
  group under a `path: null` "Deleted chats" group that expands to one row
  per chat id); and a fixed 365-day local-timezone heatmap series
  independent of `days`. The read is strictly read-only and best-effort:
  a damaged ledger is skipped for the aggregate, never quarantined from
  here.
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
- Mutations: `Mutate` — the served ops are `createSpace`, `createChat`,
  `renameChat`, `setChatConfig`, `setChatPermissionMode`, `setChatArchived`,
  `setChatPinned`, `deleteChat`, and `markChatSeen`; every other op (`renameSpace`,
  `deleteSpace`, `renameDevice`, …) falls through to `UnknownMethod`, which
  the UI surfaces as an error notice — and `QueueCommand`.
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
  counts. The card's actions (ticket 04) are read-only and separate: Review
  opens — or re-aims, since the pane keeps ONE review companion surface
  (`ui::shell`'s `RightSurface::TurnReview`) rather than stacking tabs —
  the per-file unified diff the review fetches through
  `GetCheckoutFileDiffText` in `turn` mode addressed by the Turn's
  `messageId` (a settled Turn therefore reviews its immutable stored pair,
  a live one the working tree), while Open opens the post-Turn file in the
  Space's pinned file tab. A deleted file stays reviewable but offers no
  Open, and nothing in the surface writes: no accept, undo, discard, stage,
  or commit.
- Capability surfaces the UI keeps rendered but the local backend leaves empty:
  worktrees (`CreateWorktree` / `DeleteWorktree`), change requests
  (`WatchCheckoutChangeRequest` — a stream that never emits), uploads
  (`UploadChunk` / `UploadCommit` / `ReadAttachmentChunk`), and the
  sync/account surface (`SignIn`, `SignInHeadless`, `SignOut`, `ListOrgs`,
  `CreateOrg`, `SelectOrg`, `ListRepos`, `AddRepo`, `CloneRepo`,
  `CreateRepo`, `ImportLocalWorkspace`, `ApplyUpdate`, `RetryDelivery`,
  `StopEngine`, `UpdateStatus`) — all `UnknownMethod`.

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

The provider catalog is also persisted to `provider-store.json` under the
data directory: boot writes the compiled `pi-core-rs` catalog there only when
the file is missing, and otherwise reads it. The file is the user's overlay
on the compiled catalog — providers match by id (an id that is not built-in
is dropped), models match by id within a provider (a file entry replaces the
compiled record outright, and file-only ids are appended in file order), and
a provider entry may override only `baseUrl` and `headers` (`name`,
`organizationId`, and `auth` always come from the crate). One merged catalog
is built at boot and everything answers from it — provider listing, model
listing, model resolution, context windows, eligibility, and the request
path — so a file edit needs a restart. Per-entry validation failures drop
that entry with a log line; an unparsable file, an unsupported `version`, or
an unreadable path falls back to the compiled catalog. The file is never
rewritten once it exists, and carries mode `0600` on Unix (it steers which
host the API key is sent to).

There is no separate enable toggle: `ListProviders` returns every eligible
built-in row, each carrying `configured` derived from its credential record,
and the UI filters the unconfigured rows out of its pickers. Eligibility is
`api_key` auth shaped — complex-auth providers (`amazon-bedrock`,
`azure-openai`, `cloudflare-*`, `google-vertex`, `radius`) are never offered,
and `SaveProviderKey` rejects them.

`ListProviders` groups sibling built-ins that share a `pi-core-rs`
`organization_id` (e.g. `minimax` + `minimax-cn`) into one row per
organization; the row's `variants` carry the concrete provider ids that the
key, model-list, and run RPCs address. User-defined providers stand alone —
one row each after the built-ins, `custom`-flagged in the reply, api_key-
shaped by definition, and with a reserved-id check against the boot catalog.

The catalog answers from three layers (ADR-0028): the compiled base under
the hand-edited `provider-store.json` overlay under live user entries in
`provider-settings.json`. The live layer holds bare custom model IDs added
from Settings (metadata borrowed from a template — window withheld, image
capability unknown), complete model records (first-class metadata; a
same-id record replaces the catalog entry outright, and its own `baseUrl`
is how an endpoint fix rides the request path), custom provider definitions,
and hidden model ids (excluded from listings; chats already configured with
one keep resolving it). Everything in the live layer is read per call — a
write is visible immediately — and per-entry validation drops a bad entry
with a log line while an unparsable file still fails startup. The live
layer is also what `ResetProviderCatalog` deletes: reset means the compiled
catalog under the hand-edited overlay, and credentials survive it.

Skills (ADR-0005/0006) ride the catalog above: every run whose catalog is
non-empty appends a metadata-only `<available_skills>` block to the system
prompt from a fresh three-root scan (the upstream loader and formatters;
budget-capped; an empty or fully `disable-model-invocation` catalog adds
nothing), the model self-serves `SKILL.md` through the mounted read tool, and
the composer's `/skill` slash command queues a typed `InvokeSkill` item whose
prompt the engine formats from the skill's content — resolved against a
fresh catalog when the queue admits the item, never frozen at submission —
the raw directive never reaches the model, and both invocations and
`SKILL.md` reads render as compact chips in the transcript.

The served surface is the contract above: provider configuration and
discovery, chats/spaces/sessions, the message queue, streamed transcript
frames, the git capability, the file sidebar, terminals, the usage ledger
and the device-level usage stats, Turn change sets, plan mode, and the run
loop itself. What stays deliberately
unserved is the collaboration and account surface listed above. The run loop
mounts pi-core's built-in read/write/edit/bash tools (via `engine::tools`, a
local `ExecutionEnv` rooted at the chat's cwd) plus holt's own `ls` tool and
content-search tool, the latter named `grep` (ripgrep's crates in process,
ADR-0004), the Workspace-aware
`read_chat` tool for another Chat's user-visible Transcript (ADR-0018), and the
two web tools (ADR-0023): `web_fetch` retrieves one http(s) URL and returns its
full converted text, bounded but never summarized, while `web_search` queries
the user-configured backend — resolved once per Turn admission, absent from
the toolset (not erroring) when none is configured. Neither enters the
ADR-0014 gate: fetching reads a page the way `read` reads a file. The
transcript folds their calls and results into `MessagePart::Tool` chips.
Parent runs also mount the foreground `Agent` delegation tool (ADR-0016) —
planning Turns excepted, since they run the read-only toolset. The model
setup surface (ADR-0030) lives in its own chat, not here: normal chats
mount no catalog tool at all — their catalog-write capability is nil. A
hidden `model-setup` chat (`ChatConfig.scope`, started fresh per dialog
session by `StartModelSetupChat` and deleted when the dialog closes — no
conversation memory carries across opens) runs the fixed four-step
workflow under a dedicated
system prompt, with a toolset of exactly the web tools plus the read-only
`model_proposal` (validates an exact catalog change against the local
catalog — no-op detection, deterministic checks, an optional read-only
`GET {baseUrl}/models` probe using the stored key, single-record dumps as
replacement templates — then stores it engine-side and returns a proposal
id). No file tools, no delegation, and no apply tool: the only write path
is the Settings review panel's `ApplyModelProposal` RPC, which
re-validates and transactionally applies a stored proposal — the button is
the human approval (`ListModelProposals` feeds the panel). Proposals live
per chat, in memory, capped; a restart drops them and the assistant
re-proposes. Keys never enter the path — Settings is the only key entry.

## Data on disk

Records are plain JSON or JSONL under the data directory — there is no CRDT,
operation log, or replication format anywhere in the store. Each record is
written by `crates/engine` — atomically (tmp + rename) where a whole file is
replaced — and every read path is tolerant rather than fatal: a damaged
History or usage ledger is set aside as `.corrupt`, a damaged queue is kept
and blocked instead of overwritten, and a damaged transcript opens empty
(`load_transcript` falls back to an empty Vec; the next publish replaces the
file).

- `chats.json`, `spaces.json` — the chat and space lists, whole-file atomic
  replace.
- `transcripts/<chatId>.json` — one chat document's rendered transcript
  (`Vec<SessionMessageEntry>`), rewritten whole and atomically on every
  publish, streaming frames included, under the chat's persistence lock.
  Subagent documents live under `subagents/<parentChatId>/` with the same
  layout; finished child summaries go to `subagents/<parentChatId>/results/`.
- `history/<chatId>.jsonl` — the model-facing History, append-only, one record
  per line (ADR-0010).
- `queues/<chatId>.json` — accepted queue items, rewritten whole (with an
  fsync) on every mutation, including the admission checkpoint that precedes
  any model or tool work.
- `usage/<chatId>.jsonl` — the per-chat usage ledger; `usage/archive.jsonl`
  holds the grow-only device-level archive of deleted chats.
- `turn-changes/<chatId>/<messageId>.json` — one settled Turn's frozen change
  set (ADR-0024).
- `images/` — Holt-managed pasted pixels.
- Per-feature settings records: `provider-credentials.json`,
  `provider-store.json`, `provider-settings.json`, `title-settings.json`,
  `web-search.json`, `permission-mode-default.json` — each with its own
  atomic-write and failure policy as described above.
- `device-id` (plain text), `engine.lock` (the single-instance lock), `logs/`,
  and the child `results/*.txt` summaries — the only non-JSON artifacts.

The transcript types themselves live in `crates/doc`; the engine owns every
read and write.

## Subagents

`engine::subagents` owns foreground delegation through the existing pi-core-rs
loop, without upstream changes. The fixed Explorer and Worker roles inherit
the parent Turn's model, reasoning, working directory, and Permission mode.
Explorers keep only the read-only set — `ls`, `read`, `grep`, `read_chat`,
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
requests, in the parent's tool-result History record, and the same
round-trips book into the parent chat's usage ledger as `subagent` records
stamped with the child doc id.

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

The **usage ledger** (`engine::usage`) keeps a chat's token accounting as
its own append-only JSONL record — `usage/<chatId>.jsonl`, the History
record's durability shapes (version header, repaired-append atomicity,
tolerant replay) — one line per metered provider round-trip: all token
fields (input, output, both cache fields, plus `cacheWrite1h`/`reasoning`
when reported), the attribution `kind`, provider, model, the upstream cost
stored verbatim (Holt computes no prices), and — on Turn records — the
Turn's `messageId` and `turnOutcome` (`succeeded`/`failed`/`interrupted`;
interrupted Turns keep whatever the provider reported). Every metered call
a chat causes is attributed: the loop's own rounds are `turn` records (an
assistant response and its tool results' usage fold into one), and each
auto-review pass (`auto-review`), Compaction summary (`compaction`), and
Title task (`title`) round-trip carries its own kind. A Subagent's whole
run — its own rounds, Compaction, and auto-review passes alike, one
`subagent` kind stamped with the child doc id — books into the PARENT
chat's ledger; no child ledger file exists, and the delegation tool
result's own aggregate is not double-booked. A Turn's records accumulate
in memory and land as one batch append at settlement, after queue
completion; the Title task and manual Compaction write immediately at
completion. Writes are fire-and-forget — a failed append costs only the
record, never the Turn, queue, or terminal event, and a crash before
settlement loses the batch unrepaired. A damaged file is quarantined
`.corrupt` and totals continue from zero; replay on open warms per-kind
sums plus the gross token count. Deleting a chat archives first: its whole
ledger segment, chat-attributed, appends to the device-level
`usage/archive.jsonl` (grow-only, the Usage overview's feed;
best-effort — a failed archive never blocks the delete), then the per-chat
file and its quarantined copies are removed.

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
outside the Turn model. Worktrees, change requests, uploads, and the
sync/account surface remain unserved.

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
- `LICENSE` is GPL-3.0; upstream attribution for the vendored sources rides
  that license and the README credits.
- Historical design docs for removed subsystems (sync, agent drivers, edge)
  were deleted with them; `docs/adr` keeps the decision records and
  `docs/research` keeps the UI/gpui and domain research notes.
