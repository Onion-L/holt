# App assembly and entity registry

The engine's skeleton: process assembly and the single-instance guard, the Space/Chat registry and its persistence, the entity watch streams, the Home space, and app self-update. Every other page hangs off this one.

Not here: the run loop and queue (run-loop, conversation-policy), each capability's own page, and the gpui shell chrome (`crates/ui/src/shell`, no page). This page also owns one fence: the collaboration and account RPCs the UI still renders — sign-in, orgs, repos, clone, uploads — reply `UnknownMethod` by design, and the UI surfaces that as an error notice.

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Assembly | `LocalEngine::assemble(&EngineConfig)` builds the backend the UI speaks to over the in-memory transport; another backend can replace it behind the same `RpcService` trait | [`crates/engine/src/lib.rs`](../../crates/engine/src/lib.rs) |
| Single instance | An exclusive advisory file lock on `{data_dir}/engine.lock` (`flock` on unix, std's whole-file lock — `LockFileEx` — on Windows), taken before any store opens; a second engine fails fast with the holder's pid when readable; a crash releases the lock (kernel-owned) | [`crates/engine/src/instance_lock.rs`](../../crates/engine/src/instance_lock.rs) |
| Registry persistence | `spaces.json` / `chats.json`, whole-file atomic replace (tmp + rename); a missing file loads empty, a malformed one fails assembly | [`crates/engine/src/store.rs`](../../crates/engine/src/store.rs) |
| Home space | `HOME_SPACE_ID` is always present and can never be created, deleted, or renamed (refused); a chat created without a space lands in it; at boot, chats with no space are adopted into it and given the Home folder as cwd | [`crates/engine/src/rpc/spaces.rs`](../../crates/engine/src/rpc/spaces.rs), `lib.rs` |
| Entity mutations | `Mutate` ops: create/delete/rename Space, createChat, renameChat, setChatConfig, setChatArchived, setChatPinned, deleteChat, markChatSeen; deleting a chat cascades into every per-chat store (transcript, history, queue, usage, turn-changes, subagents) | [`crates/engine/src/rpc/spaces.rs`](../../crates/engine/src/rpc/spaces.rs) |
| Entity watches | `WatchChats` / `WatchSpaces` / `WatchSessions` / `WatchDevices` / `WatchConnectivity`: broadcast channels with a snapshot on subscribe; devices watch answers the local machine's row only, connectivity one static Disabled frame | [`crates/engine/src/rpc.rs`](../../crates/engine/src/rpc.rs) |
| Device identity | The stable device id in plain-text `device-id` under the data dir; mints checkout identities (see workspace-git) | `store.rs`, `lib.rs` |
| Self-update | Polls the latest GitHub release every 6 h (a failed check backs off 15 min, doubling to that cap); `CheckUpdate` runs one round on demand — inline when no loop is running (a prerelease build). Any non-prerelease build reports availability, but only a managed install can apply — detected per OS: the `.app` bundle layout (macOS), a `holt-release.marker` beside the exe (Windows), `$APPIMAGE` naming the running file (Linux) — and `UpdateStatus.managed` says which this is. On apply, downloads this arch's asset with streamed progress (cancellable until the install starts), checks its sha256 against the release's `manifest.json` where the platform requires one, then swaps in place per platform — Windows unzips and renames the staged exe over the running one (the displaced `.old` is deleted at next start), Linux renames the verified file over the AppImage, macOS keeps the `hdiutil`/`codesign`/`ditto` chain and its Developer ID requirement; restarting is the UI's job | [`crates/engine/src/update.rs`](../../crates/engine/src/update.rs), [`crates/engine/src/update_platform.rs`](../../crates/engine/src/update_platform.rs) |
| UI cutaways | The sidebar's chat list and space switching consume these watches through the state layer's watch coordinator | [`crates/ui/src/state.rs`](../../crates/ui/src/state.rs), [`history.rs`](../../crates/ui/src/history.rs) |

## Runtime path

1. **Boot.** The binary starts the UI, which assembles the engine from `EngineConfig` (data dir, optional test seams) and connects a memory `RpcClient`. The instance lock is taken first; a second process on the same data dir exits with the holder's pid. Chats with no space are adopted into the Home space before the registry is served.
2. **Mutate.** Registry writes replace the whole JSON file atomically and bump the matching watch channel; every subscriber's next frame is the full new list.
3. **Watch.** A subscriber gets one snapshot frame, then frames on change — no diffs, no replay.
4. **Delete.** Deleting a space refuses the Home space and pauses its routines; deleting a chat archives its usage to the device ledger, then removes every per-chat file and directory the other pages own.
5. **Update.** A background poll — every 6 h, backing off 15 min → 6 h while checks fail — or a `CheckUpdate` round on demand fetches the latest release plus its `manifest.json`; Windows/Linux are offered the update only when the manifest carries a sha256 for this arch's asset (macOS does not need one — its gate is the DMG's code requirement, with the digest still checked when present). `ApplyUpdate` — a managed install only — streams the download, cancellable until the install starts, hashing it against that digest, then swaps in place per platform: the staged exe is renamed over the running one on Windows, the verified file over the AppImage on Linux, the verified bundle over the `.app` on macOS. A cancel or failure leaves the running install as it was; restarting onto the new build is the UI's job.

## Constraints, state, and failure behavior

- **One engine per data dir, ever.** The lock precedes every store open; nothing else in the process may write the data dir before it is held.
- **Registry files are whole-file truth.** No partial writes, no migration journals; a file that will not parse fails startup loudly rather than serving a subset.
- **The Home space is a floor, not a row.** It cannot be removed by any mutation path, so "a chat with nowhere to go" is unreachable — that is what makes chat-less defaults (the new-chat canvas, terminals without a chat) well-defined.
- **The unserved surface is deliberate.** `UnknownMethod` for the sync/account family is the contract, not a regression; the UI keeps those surfaces rendered.
- **Update fails closed.** Windows and Linux are not offered an update at all unless the release's `manifest.json` carries a sha256 for this arch's asset, and a digest mismatch aborts with nothing replaced; macOS's gate is unchanged — nothing installs without the Developer ID code-requirement match. A build that is not a managed install still reports availability but cannot apply, and a cancelled or failed update leaves the running install as it was (a half-done swap puts the old file back).

## Verification

```sh
cargo test -p holt-engine store
cargo test -p holt-engine instance
cargo test -p holt-engine update
cargo test -p holt-engine space
```

The suites cover atomic registry persistence and tolerant loads, lock refusal for a second holder, Home-space adoption at boot, refusal of Home-space deletion/rename, and the unit-tested self-update pieces: the per-platform managed-install layouts, the release-asset naming, the file swap's replace / stale-backup / rollback cases, hex digest comparison against a known answer, `manifest.json` parsing, the failure-backoff ladder, numeric version compare, and the `CheckUpdate` round handshake. Not covered by them: the live network round, the download/cancel/progress state machine, and the macOS `codesign`/`hdiutil` chain; the Windows semantics of renaming a *running* exe are left to real-machine verification (noted in `update_platform.rs`).
