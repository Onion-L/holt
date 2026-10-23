# Workspace files

The far-right file sidebar and everything it serves: fenced workspace listings, versioned text reads and saves, entry management, pasted-image handling, and the composer's `@` path search. The UI never touches the workspace filesystem — every boundary rule is enforced engine-side before any filesystem work.

Not here: git-derived views (see workspace-git — though the sidebar's status feed is computed there); the agent's file tools (see toolset-mechanics — the sidebar is the user's surface); unsaved-draft ownership across chats (Space-owned state, see shell-registry's fence).

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| The fence | Root containment, `.git` exclusion (gitdir pointer files too), and symlink rules — resolved and enforced on every request, engine-side. A read may additionally land inside the personal or Holt skill roots so transcript skill chips can open a `SKILL.md`; saves and every other verb keep the plain fence | [`crates/engine/src/files.rs`](../../crates/engine/src/files.rs) (`resolve_inside_root`, `exclude_git`) |
| Listings | `ListWorkspaceEntries`: one directory level, directories first, hidden entries in, `.git` out, symlink kinds resolved against the root, capped per directory (10,000 rows) | `files.rs` (`list_directory`) |
| Text reads | `ReadWorkspaceFile`: UTF-8 up to 2 MiB plus BOM and line-ending facts and an opaque disk `version` token; oversized, non-UTF-8, and binary files answer a typed unsupported reason, never a lossy decode | `files.rs` (`read_file`) |
| Saves | `SaveWorkspaceFile`: version-checked atomic write preserving permissions — a moved disk version is a `versionConflict` reply, never an overwrite, and a missing file is never silently recreated. `WriteWorkspaceFileAs` creates new in-root destinations only; collisions refuse, originals untouched | `files.rs` (`save_file`, `write_file_as`) |
| Entry management | Create (atomic no-clobber, valid names), rename (the entry itself — a symlink renames as a link, never its target; noreplace is native on macOS and Windows — `renamex_np`, `MoveFileExW` — and a pre-checked rename elsewhere; case-only passes), move (one rename into an existing in-root directory — never a copy/delete pair, so a cross-device move fails instead of degrading; refuses outside-root, subtree, and self destinations), trash | `files.rs` (`create_entry`, `rename_entry`, `move_entry`) |
| Trash | `TrashWorkspaceEntry` goes through the operating system's trash and nothing else: an unavailable or refusing trash fails — there is no permanent-delete fallback anywhere | [`crates/engine/src/trash.rs`](../../crates/engine/src/trash.rs) |
| Managed images | A managed image is a path provider, not an upload: pasted bytes land under `<data_dir>/images/<uuid>.<ext>` once, durably, before the referencing message is acknowledged, and the file never moves — the path in a draft, queue row, Transcript, or History stays valid. Four formats end to end (PNG, JPEG, GIF, animated WebP — first frame only); a hard byte cap is enforced before the bytes move. External picker/drag files are never cleanup candidates | [`crates/engine/src/images.rs`](../../crates/engine/src/images.rs) |
| Image release | `ReleaseImage` reclaims only managed files without retained references (drafts, durable queues, Transcript, History); startup reconciles durable references before serving image RPCs, reclaiming abandoned images and images of deleted chats — unreadable records retain conservatively | `images.rs` |
| Capability flags | `ModelInfo.imageCapability` is `supported` / `unsupported` / `unknown`: built-ins derive it from the provider catalog, custom models stay unknown. Known non-visual models show a composer limitation while still accepting paths; unknown models may attempt real requests — rejection follows normal Turn failure, with no automatic model switch | [`crates/proto/src/agent.rs`](../../crates/proto/src/agent.rs) |
| Path search | `SearchFiles`: fuzzy matches on full workspace-relative paths (so `cmp/send` finds `composer/send.rs`) for the composer's `@` popup — hidden files searched, ignore rules not applied, contents never cross this boundary; results capped. Deliberately broader than the agent's content search, which keeps its ignore-respecting traversal | [`crates/engine/src/path_search.rs`](../../crates/engine/src/path_search.rs) |
| Live tree | `WatchWorkspaceEntries`: one recursive watcher per subscription root, armed on the blocking pool, the same 200 ms quiet debounce as the git watch; frames carry the absolute changed paths so the UI re-lists affected directories and re-reads clean open files. Dirty buffers are the UI's business — the stream only reports what moved on disk; dropping it ends the watch | [`crates/engine/src/workspace_watch.rs`](../../crates/engine/src/workspace_watch.rs) |
| Folder browsing | `ListFolders` / `ListDrives` for the add-space palette (macOS: the system root plus `/Volumes`; Windows: the fixed, removable, and network drive letters), and `ReadImage` — the bounded read behind UI previews (any local path, unlike the fenced sidebar verbs) | [`crates/engine/src/local_fs.rs`](../../crates/engine/src/local_fs.rs), `images.rs` |
| UI cutaways | The file tree, editor tabs, previews, and image surface | [`crates/ui/src/files/`](../../crates/ui/src/files) |

## Runtime path

1. **List.** The sidebar asks one directory level per row expansion; the engine resolves the fence, classifies, orders, and caps.
2. **Open and edit.** A read returns text + facts + a version token; the editor's save sends the reviewed token back, and a moved version refuses instead of overwriting what the user never saw.
3. **Watch.** Disk changes arrive as changed-path frames; clean open files re-read, dirty ones surface a conflict state the user resolves against the reviewed token.
4. **Paste.** Pixels stage durably first; the message carries only the stable path, and previewing or submitting a path never adds pixels to a model request (only the agent's `read` tool does that — see toolset-mechanics).

## Constraints, state, and failure behavior

- **One path vocabulary**: every path this surface hands the UI — listing rows and directory keys, read replies, symlink `resolved_path`s, rename/move destinations, watch frames — is the addressed spelling of the canonical path (`tools::addressed_canonical`: Windows's `\\?\` verbatim prefix dropped, `/` separators; identity elsewhere). The UI string-matches tabs ↔ tree rows ↔ frames ↔ resolved identities, so a producer spelling its paths differently (a raw `display()` of a canonicalized path) breaks tab association and re-read matching on Windows. Outside this surface the spelling is per-producer: the folder palette (`ListFolders`/`ListDrives`) carries native paths, and the git status `workdir` is addressed (see workspace-git).
- **The fence is the product**: every fault is a typed reason the UI renders as a distinct state — outside-root, `.git`-excluded, not-found, collision, invalid destination — never a silent best-effort.
- **No destructive verb exists**: trash-or-fail, version-check-or-refuse, create-no-clobber; nothing in this surface can overwrite or permanently delete user data.
- **A managed path is forever**: files never move after staging, so references in persisted records stay valid; drafts themselves are not persisted, and startup reconciliation is the only reclaimer.
- **Bounds are honest**: a file past a bound (2 MiB text, 25 MiB image, 10k entries, 50 matches) answers a reason, not a truncated guess.

## Verification

```sh
cargo test -p holt-engine files
cargo test -p holt-engine images
cargo test -p holt-engine path_search
cargo test -p holt-engine --test all workspace
```

The suites cover the fence (outside-root refusals, gitdir pointer exclusion, symlink classification, the skill-root read exception), BOM/line-ending preservation, unsupported-reason taxonomy, version-conflict refusals, rename/move semantics (symlink-as-link, noreplace, subtree refusal), trash without fallback, managed-image staging/release/reconciliation, and the fuzzy path matcher's contract.
