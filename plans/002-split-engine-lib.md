# Plan 002: Split `engine/lib.rs` into `agent` / `rpc` / `store` / `local_fs`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in "STOP conditions" occurs, stop and report — do
> not improvise. When done, update your row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 5c2f3f4..HEAD -- crates/engine/src/lib.rs`
> — written against `5c2f3f4`, where the file is exactly 1663 lines. On any
> difference or excerpt mismatch, STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW (all four steps are pure code movement + one doc rewrite)
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `5c2f3f4`, 2026-09-01

## Why this matters

`crates/engine/src/lib.rs` (1663 lines, 1123 of them code) is the most
actively evolving file in the repo (4 commits since `34a9a9d`, including
`c00b0fa` and `2a102e0`), yet its module doc still claims it is an empty
stub. It actually contains five concerns: engine assembly/config, a real
single-agent LLM run loop over `pi-core`, JSON persistence, local
filesystem browsing, and the `RpcService` dispatch. Per `ARCHITECTURE.md`
(line 15: "A real backend replaces the stub behind the same [trait]"),
splitting dispatch / agent / persistence now makes that future replacement a
matter of swapping `rpc.rs` internals instead of dissecting a monolith.
The crate's external surface is exactly two items —
`crates/ui/src/state.rs:31`: `use holt_engine::{EngineConfig, StubEngine};`
— both of which stay in `lib.rs`, so the split changes nothing for the UI.

## Current state

`crates/engine/src/` already has `credentials.rs`, `instance_lock.rs`,
`provider_settings.rs`, `providers.rs` as `pub mod`s declared from lib.rs:51–54.
`lib.rs` layout at `5c2f3f4`:

| Lines | Content |
|---|---|
| 1–13 | module doc (STALE — describes an empty stub; rewritten in Step 4) |
| 15–59 | imports + `pub mod` declarations + `use` of the existing four modules |
| 61–75 | `EngineError`, `EngineConfig` |
| 77–88 | `pub struct StubEngine` (6 private fields: `engine_info`, `data_dir`, `spaces`, `spaces_tx`, `runtime`, `providers`, `_instance_lock`) |
| 90–181 | `ChatRuntime` (+impl), `AgentRuntime` (+impl) |
| 183–217 | `impl StubEngine`: `assemble`, `engine_info` (STAY in lib.rs) |
| 219–493 | `impl StubEngine`: `watch_spaces`, `watch_value`, `create_space`, `create_chat`, `queue_command`, `set_chat_config` |
| 496–527 | `CreateSpaceParams`, `CreateChatParams`, `QueueCommandParams` (private Deserialize structs) |
| 529–549 | `provider_reasoning`, `required_string` |
| 551–613 | `user_agent_message`, `assistant_parts`, `update_assistant_entry` |
| 615–770 | `AgentRun`, `run_agent_command` (the LLM run loop) |
| 772–820 | `spaces_path`, `chats_path`, `load_chats`, `persist_chats`, `load_spaces`, `persist_spaces` |
| 822–836 | `static_watch`, `pending_stream` |
| 838–885 | `local_device`, `hostname`, `home_dir`, `expand_tilde` |
| 887–965 | `FOLDER_ENTRY_CAP`, `list_folders`, `list_drives` |
| 967–1107 | `impl RpcService for StubEngine` (~40-arm dispatch) |
| 1109–1121 | `load_or_create_device_id` |
| 1123–1663 | `mod tests` (14 tests, `use super::*`) |

- Excerpt — the struct (stays in lib.rs):

```rust
// lib.rs:77
pub struct StubEngine {
    engine_info: EngineInfo,
    data_dir: PathBuf,
    spaces: RwLock<Vec<Space>>,
    spaces_tx: watch::Sender<serde_json::Value>,
    runtime: Arc<AgentRuntime>,
    providers: Arc<ProviderAdapter>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}
```

- Wiring convention for this crate: the new modules are **private** `mod`s at
  the crate root with `pub(crate)` items, imported into `lib.rs` with plain
  `use` lines. `rpc.rs` opens a second `impl StubEngine` block and reads the
  struct's private fields directly — legal because child modules see the
  parent's private items (the same trick `crates/ui/src/shell/spaces.rs:201`
  uses). Trait impls (`impl RpcService for StubEngine`) are globally visible;
  nothing needs re-exporting for `state.rs` to keep working.
- The 14 tests split 9/5: nine integration tests exercise the engine through
  `StubEngine::assemble` + `handle` and STAY in lib.rs; five unit tests move
  with their functions (listed per step). lib.rs's `mod tests` keeps
  `use super::*` and needs no new imports after the five moves (verified
  against every remaining test's references).

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt -p holt-engine -- --check` | exit 0, no output |
| Format fix | `cargo fmt -p holt-engine` | exit 0 |
| Lint | `cargo clippy --workspace` | exit 0 (baseline dep warnings only) |
| Tests (engine) | `cargo test -p holt-engine` | `test result: ok. 24 passed; 0 failed` |
| Tests (consumers) | `cargo test -p holt-ui` | 525 passed (proves the crate API held) |

**Do not run `cargo fmt --all`** — broken by the vendored gpui workspace
(plans/README.md). Use `-p holt-engine`.

## Scope

**In scope**:
- `crates/engine/src/lib.rs` (modify — becomes the facade)
- `crates/engine/src/store.rs` (create)
- `crates/engine/src/local_fs.rs` (create)
- `crates/engine/src/agent.rs` (create)
- `crates/engine/src/rpc.rs` (create)

**Out of scope**:
- `crates/engine/src/{credentials,instance_lock,provider_settings,providers}.rs`
  — existing modules, untouched.
- `ARCHITECTURE.md` — its `crates/engine` row (line 24) describes behavior,
  not file layout, and stays accurate after the split.
- Any behavior change: the run loop, persistence format, RPC replies, error
  strings must stay byte-identical. No renames, no signature changes, no
  "improvements" to the code being moved.

## Git workflow

- Worktree `../holt-w002`, branch `refactor/002-split-engine`
  (`git worktree add ../holt-w002 -b refactor/002-split-engine 5c2f3f4`
  from the repo root; `export CARGO_TARGET_DIR=/Users/onion/workbench/holt/target`).
- One commit per step:
  - C1 `refactor(engine): move JSON persistence into store module`
  - C2 `refactor(engine): move local filesystem browsing into local_fs module`
  - C3 `refactor(engine): move the agent run loop into agent module`
  - C4 `refactor(engine): move RPC handlers and dispatch into rpc module`
- **Before every commit**, run the review gate in plans/README.md.

## Steps

Every step is a pure move: cut the listed items from `lib.rs`, paste
verbatim into the new file (with the imports that file needs), add the
`mod`/`use` wiring to `lib.rs`, adjust visibility ONLY as listed. Each step
compiles green with all 24 engine tests passing. Rule for imports in both
old and new files: add what the compiler says is missing, remove what it
warns is unused — nothing else.

### Step 1: `store.rs` → commit C1

1. Create `crates/engine/src/store.rs`:
   - Header: `//! JSON persistence for spaces/chats and the stable device id,
     written atomically (tmp + rename) under the data dir.`
   - Move (verbatim): `spaces_path` (772–774), `chats_path` (776–778),
     `load_chats` (780–789), `persist_chats` (791–799), `load_spaces`
     (801–810), `persist_spaces` (812–820), `load_or_create_device_id`
     (1109–1121).
   - Mark all seven `pub(crate) fn` (used by lib.rs's `assemble` and by
     rpc.rs handlers once they move).
   - Imports it needs: `std::path::{Path, PathBuf}`, `holt_proto::{Chat, Space}`,
     `use crate::EngineError;` (plus `serde_json` — already an extern crate
     used via full path; keep the code's existing `serde_json::` paths).
2. In `lib.rs`: add `mod store;` next to the existing `pub mod` block, and

```rust
use store::{
    load_chats, load_or_create_device_id, load_spaces, persist_chats, persist_spaces,
};
```

   (`persist_chats`/`persist_spaces` are still called by lib.rs's own
   handlers until Step 4 — that's expected; Step 4 prunes them on warning.)
3. Delete the moved source from lib.rs.

**Verify**: fmt → clippy → `cargo test -p holt-engine` → **24 passed**.
Review gate, commit C1.

### Step 2: `local_fs.rs` → commit C2

1. Create `crates/engine/src/local_fs.rs`:
   - Header: `//! Local-machine surfaces for the add-space palette: folder
     browsing, mounted volumes, hostname, and the local device row.`
   - Move: `local_device` (838–849), `hostname` (851–865), `home_dir`
     (867–871), `expand_tilde` (873–885), `const FOLDER_ENTRY_CAP` (887–889),
     `list_folders` (891–936), `list_drives` (938–965).
   - Visibility: `local_device`, `list_folders`, `list_drives` become
     `pub(crate) fn`; `hostname`, `home_dir`, `expand_tilde`,
     `FOLDER_ENTRY_CAP` stay private (only used within this file and its
     tests).
   - Move 4 tests (lib.rs:1151–1198):
     `list_folders_returns_dirs_sorted_and_marks_repos`,
     `list_folders_errors_read_like_folder_failures`,
     `expand_tilde_resolves_against_home`,
     `list_drives_always_offers_the_system_root` into
     `#[cfg(test)] mod tests { use super::*; … }`.
   - Imports: `std::path::Path`, `holt_proto::{Device, DriveEntry,
     DriveListing, FolderEntry, FolderListing}` (plus the `libc` call in
     `hostname` works as-is — `libc` is a dependency of this crate).
2. In `lib.rs`: add `mod local_fs;` and
   `use local_fs::{list_drives, list_folders, local_device};` (the
   `RpcService` impl still lives in lib.rs until Step 4 and calls all three).
3. Delete moved source; move the 4 tests out of lib.rs's `mod tests`.

**Verify**: fmt → clippy → `cargo test -p holt-engine` → **24 passed**
(20 remain in lib.rs, 4 now in local_fs.rs). Review gate, commit C2.

### Step 3: `agent.rs` → commit C3

1. Create `crates/engine/src/agent.rs`:
   - Header: `//! The single-agent run loop over pi-core: per-chat runtime
     state, event-to-transcript translation, and history persistence.`
   - Move: `ChatRuntime` struct + impl (90–115), `AgentRuntime` struct + impl
     (117–181), `provider_reasoning` (529–541), `user_agent_message`
     (551–557), `assistant_parts` (559–586), `update_assistant_entry`
     (588–613), `AgentRun` (615–626), `run_agent_command` (628–770).
   - Visibility: `ChatRuntime`, `AgentRuntime`, `AgentRun` become
     `pub(crate) struct`; `run_agent_command` becomes `pub(crate) async fn`;
     `provider_reasoning`, `user_agent_message`, `assistant_parts`,
     `update_assistant_entry` stay private.
   - Move 1 test (lib.rs:1585–1615):
     `assistant_message_maps_text_and_reasoning_to_doc_parts` (it uses
     `assistant_parts` + pi_core types; `use super::*` covers both).
   - Imports it needs (carve from lib.rs's existing use blocks):
     `std::sync::{Arc, Mutex, RwLock}`, `chrono::Utc`, `holt_doc::{MessagePart,
     MessageRole, MessageStatus, SessionMessageEntry, TranscriptFrame}`,
     `holt_proto::{Chat, ReasoningLevel, Session, SessionStatus}`, the whole
     `pi_core::{agent::{…}, ai::{…}}` block (lib.rs:33–46),
     `tokio::sync::watch`, `tokio_util::sync::CancellationToken`.
2. In `lib.rs`: add `mod agent;` and
   `use agent::{AgentRun, AgentRuntime, ChatRuntime, run_agent_command};`
   (`AgentRun`/`run_agent_command` are used by lib.rs's `queue_command`
   until Step 4; prune on warning then).
3. Delete moved source; move the 1 test.

**Verify**: fmt → clippy → `cargo test -p holt-engine` → **24 passed**.
Review gate, commit C3.

### Step 4: `rpc.rs` + facade finish → commit C4

1. Create `crates/engine/src/rpc.rs`:
   - Header: `//! The RPC surface: `RpcService` dispatch plus the space/chat
     mutation and queue-command handlers it routes to.`
   - Move: the handler section of `impl StubEngine` (219–493):
     `watch_spaces`, `watch_value`, `create_space`, `create_chat`,
     `queue_command`, `set_chat_config` — as a new
     `impl StubEngine { … }` block, all methods stay private (only the
     `RpcService` impl in this same file calls them).
   - Move: `CreateSpaceParams`/`CreateChatParams`/`QueueCommandParams`
     (496–527), `required_string` (543–549), `static_watch` (822–830),
     `pending_stream` (832–836) — all stay private.
   - Move: `impl RpcService for StubEngine` (967–1107) verbatim.
   - Imports: `async_trait::async_trait`, `serde::Deserialize`,
     `serde_json`, `chrono::Utc`, `holt_doc::{MessagePart, MessageRole,
     SessionCommandPayload}`, `holt_proto::{AuthState, ChatConfig}`,
     `holt_rpc::{RpcError, RpcReply, RpcService, methods}`,
     `tokio::sync::watch`, `use crate::StubEngine;`,
     `use crate::agent::{AgentRun, run_agent_command};`,
     `use crate::local_fs::{list_drives, list_folders, local_device};`,
     `use crate::store::{persist_chats, persist_spaces};`,
     `use crate::providers::ProviderAdapter;` (for
     `ProviderAdapter::is_eligible`).
   - These handlers read `StubEngine`'s private fields (`self.spaces`,
     `self.spaces_tx`, `self.runtime`, `self.providers`, `self.data_dir`,
     `self.engine_info`) directly — that is intended and compiles (child
     module of the crate root). Do not change field visibility.
2. In `lib.rs`:
   - Add `mod rpc;` (no `use` line — nothing in lib.rs calls these).
   - Prune now-unused imports (expected: `async_trait`, `Deserialize`,
     `holt_rpc::{RpcError, RpcReply, RpcService, methods}`, `holt_proto`
     items now only used in children, `CancellationToken`, most of `pi_core`)
     — trust the compiler's unused-import warnings, remove exactly those.
   - Rewrite the module doc (lines 1–13) to:

```rust
//! holt-engine — the in-process backend for the desktop shell.
//!
//! - [`StubEngine`] — the [`RpcService`] the UI speaks to over the
//!   in-memory RPC transport: space/chat persistence, watch streams,
//!   provider discovery and credentials, folder browsing, and a
//!   single-agent LLM run loop over pi-core. A real backend replaces it
//!   behind the same trait: implement the methods in its `handle`, keep
//!   the reply shapes, and the whole UI keeps working.
//! - [`InstanceLock`] — single-instance guard on the data dir.
//! - module map: `agent` (run loop + runtime state), `rpc` (dispatch +
//!   handlers), `store` (JSON persistence), `local_fs` (folder browsing),
//!   plus provider discovery and Holt-owned credential storage behind the
//!   RPC seam.
```

3. Check the tail state of lib.rs: module doc, imports, `EngineError`,
   `EngineConfig`, `StubEngine`, `impl StubEngine { assemble, engine_info }`,
   the four existing `pub mod`s + four new private `mod`s, and `mod tests`
   with the 9 remaining integration tests. Target: lib.rs ≤ ~400 lines.

**Verify**: fmt → clippy → `cargo test -p holt-engine` → **24 passed** →
`cargo test -p holt-ui` → **525 passed** (proves `state.rs`'s import held).
`grep -n "use holt_engine" crates/ui/src/state.rs` unchanged.
Review gate, commit C4.

## Test plan

No new tests. The crate's 24 tests at baseline decompose as 15 in lib.rs +
9 in the pre-existing `credentials`/`providers`/`instance_lock`/
`provider_settings` modules (untouched). Of lib.rs's 15: **5 move** with
their functions (4 listed in Step 2, 1 in Step 3) and **10 stay**
(`device_id_is_stable_across_assembles`,
`second_engine_on_one_data_dir_fails`,
`create_space_updates_watch_and_survives_restart`,
`provider_catalog_uses_provider_qualified_model_ids`,
`custom_provider_model_is_listed_resolved_and_persisted`,
`provider_credential_rpc_keeps_secrets_out_of_catalogs`,
`provider_catalog_groups_variants_by_organization`,
`run_against_unconfigured_variant_is_rejected`,
`removing_a_key_preserves_persisted_chat_selection`,
`create_chat_updates_chat_and_transcript_watches`). The gate is the count:
24 passed at every commit, no test edited.

## Done criteria

- [ ] `cargo fmt -p holt-engine -- --check` exits 0
- [ ] `cargo clippy --workspace` exits 0 (baseline warnings only)
- [ ] `cargo test -p holt-engine` → 24 passed, 0 failed
- [ ] `cargo test -p holt-ui` → 525 passed, 0 failed
- [ ] `wc -l crates/engine/src/lib.rs` reports ≤ 450
- [ ] `ls crates/engine/src/` contains `agent.rs`, `rpc.rs`, `store.rs`,
      `local_fs.rs` alongside the four pre-existing modules
- [ ] `git diff 5c2f3f4..HEAD -- crates/ui crates/rpc crates/doc crates/theme crates/proto`
      is empty (no consumer touched)
- [ ] `git status --short` shows no files outside the In-scope list
- [ ] 4 commits on `refactor/002-split-engine`, each review-gated
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check fails, or any line range/excerpt doesn't match.
- A moved function fails to compile for any reason other than a missing
  `use` import (adding imports is allowed; changing a moved function's
  body/signature/strings is not).
- The 24-test or 525-test count changes at any commit.
- `cargo test -p holt-ui` fails at Step 4 — means the facade lost an item
  `state.rs` needs (`EngineConfig`/`StubEngine` must remain importable from
  the crate root exactly as before).
- The reviewer rejects a commit twice.

## Maintenance notes

- When the real backend lands (pi-core-rs proper), the swap point is
  `rpc.rs`'s `impl RpcService` and `agent.rs`'s `run_agent_command`;
  `store.rs` (persistence) and `local_fs.rs` (folder browsing) are reusable
  as-is.
- The module doc rewrite in Step 4 intentionally drops the word "stub" from
  the description of the run loop — the code runs real LLM turns; if
  someone reintroduces "stub" language, `ARCHITECTURE.md` line 24 is the
  doc to keep in sync.
- Reviewers: expect `rpc.rs` to touch `StubEngine`'s private fields
  directly; that is the crate's established child-module pattern, not a
  encapsulation bug.
