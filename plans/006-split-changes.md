# Plan 006: Split the Changes viewer into cohesive UI modules

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving on. This
> is a move-first structural refactor: preserve the existing Changes behavior,
> public paths, rendered strings, RPC payloads, and tests. If a STOP condition
> occurs, stop and report instead of improvising. When done, update the plan
> 006 row in `plans/README.md` unless a reviewer is maintaining the index.
>
> **Drift check (run first)**: `git diff --stat 9b20de6..HEAD -- crates/ui/src/changes.rs crates/ui/src/changes crates/ui/src/lib.rs plans/README.md`
> The planned baseline is commit `9b20de6`, where `changes.rs` is 5,517 lines
> and the working tree is clean. If any in-scope path changed, compare every
> symbol in the move manifest below with the live code before editing; a
> mismatch is a STOP condition until the manifest is re-derived.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `9b20de6`, 2026-09-04

## Why this matters

`crates/ui/src/changes.rs` is a 5,517-line module containing five independent
change axes: pure patch parsing and diff-domain values, the virtualized row and
fold model, watch/scoped RPC synchronization, comment drafting, and all GPUI
rendering. This makes unrelated Changes work compete in one merge surface and
forces pure parser/row behavior to live beside entity lifecycles and paint
code. Splitting those responsibilities into a module directory reduces merge
conflicts and makes the existing characterization tests and pure helpers easier
to reuse, without changing the `crate::changes::{...}` API or the UI behavior.

This plan deliberately does not redesign the diff viewer. The executor must
retain checkout-id/device+cwd/cwd resolution, preparing/clean/list states, the
watch error banner, line-granular `list()` virtualization, body-row removal on
fold, 180 ms fold and 200 ms chevron transitions, background paint-only syntax
highlighting, Working tree/Branch changes/Latest turn/History/Commit scopes,
and unified/split flattening semantics.

## Current state

- `crates/ui/src/changes.rs` — public Changes module, shared layout constants,
  pure diff model/parser, row flattening, the `Changes` entity and all watch,
  RPC, history, fold, comment, menu, highlight, and GPUI render code. The
  entity starts at line 1403; rendering starts at `render_row` line 2688 and
  `impl Render for Changes` line 4363; tests start at line 4525.
- `crates/ui/src/lib.rs` — declares `pub mod changes;`; keep this declaration
  and every existing public `crate::changes::...` path unchanged. Rust's
  module-directory form will use `crates/ui/src/changes/mod.rs` after the
  conversion; no caller should need a path rewrite.
- `crates/ui/src/shell.rs` and `crates/ui/src/shell/right_pane.rs` — construct
  `Changes`/`ChangesEvent` and call `ensure_content`; these shell-facing types
  must remain re-exported from the facade.
- `crates/ui/src/transcript/tool.rs` — consumes `FileDiff`, `DiffLine`,
  `FileStatus`, `Hunk`, `LineKind`, `DIFF_LINE_HEIGHT`,
  `truncate_file_lines`, `body_height`, and
  `render_file_body_with_syntax` for inline tool diffs.
- `crates/ui/src/transcript/render.rs` — consumes `DiffHighlights` and
  `render_file_body_with_syntax` for paint-only syntax runs. These consumers
  are the compatibility boundary for the public/restricted re-exports.

### Move manifest (derived at `9b20de6`)

The following symbols are the intended ownership after the split. Move bodies
verbatim; only imports, `super::` qualification, module declarations, explicit
re-exports, and compiler-required visibility keywords may differ.

1. `crates/ui/src/changes/model.rs` — pure diff domain and parsing:
   - `LineKind`, `DiffLine`, `SourceSide`, `SourceLineRef`, `DiffHighlights`
     and its methods; `Hunk`, `FileStatus`, `FileDiff`, `FileDiff::new`.
   - `gutter_width`, `strip_git_prefix`, `parse_git_paths`,
     `parse_hunk_header`, `parse_patch`, `file_notices`,
     `truncate_file_lines`.
   - `LinePair`, `split_pairs`, `split_pairs_upto`, `pair_anchors`,
     `line_anchor`.
   - `resolve_diff`, `DiffPhase`, `diff_phase`, `uncommitted_label`,
     `DiffScope` and its methods, `scope_label`, `default_base_ref`,
     `clean_message`, and `apply_diff_frame`.
   - `comment_state_key`, `hash64`, `MAX_EXCERPT_SOURCE_LINES`,
     `excerpt_side`, `excerpt_highlights`, `sources_match_patch`, and
     `full_highlights`. Keep syntax work pure and keep all existing caps and
     stale-source checks.
2. `crates/ui/src/changes/rows.rs` — pure virtualized row and sticky-header
   model:
   - `DiffRow` and `DiffRow::height`, `body_row_count`, `body_rows`,
     `flatten_rows`, `body_height`, and `body_height_with`.
   - `StickyFileHeader`, `sticky_file_header`,
     `sticky_header_push_offset`, `FileHeaderPresentation` and its methods,
     `StickyFileHeaderPaint`, and `sticky_file_header_paint`.
   - `FileFold` and its `animating` method stay in the facade with the
     entity-owned state below so `sync` and `render` do not need a broad
     cross-module visibility surface. `FOLD_TWEEN_WINDOW` and
     `FOLD_TWEEN_MAX_PX` likewise stay in the facade. `ParsedDiff` stays there
     because it is entity-owned parsed state.
3. `crates/ui/src/changes/sync.rs` — `impl Changes` methods that coordinate
   engine state and row/list state:
   - `ensure_watch`, `spawn_watch`, `resolved`, `scoped_cwd`, `active_diff`,
     `scope_key`, `parse_key`, `ensure_branches`, `ensure_scoped`,
     `set_scope`, `history_pane`, `history_count`,
     `history_fetch_button`, `set_base_ref`, `ensure_content`, and `sync`.
   - `replace_file_body`, `toggle_fold`, `ensure_fold_settle`, `settle_folds`,
     `all_collapsed`, `toggle_collapse_all`, `toggle_mode`, and `reflatten`.
     Preserve every `ListState::splice/reset` operation and the key checks that
     discard late async results.
4. `crates/ui/src/changes/comments.rs` — comment interaction state and methods:
   - `HoverRow`, `CommentDraft`, `staged_comments`, `comments_for`,
     `old_path_of`, `discard_stale_draft`, `draft_anchor`, `draft_anchor_in`,
     `sync_comment_rows`, `set_hover`, `hovering`, `clear_hover_at`,
     `open_draft`, `cancel_draft`, `commit_draft`, and `remove_comment`.
     Preserve the composer-key checkout guard, old-path citations, and the
     single-draft invariant.
5. `crates/ui/src/changes/render.rs` — GPUI element construction and rendering:
   - `request_highlight` and the GPUI rendering state transitions. The
     entity-owned `HighlightSlot`, `DiffHighlightState`, and `RefMenu` structs
     stay in the facade; their task handles remain fields on `Changes`.
   - `render_row`, `render_file_header`, `render_sticky_file_header`,
     `header_button`, `header_toggle`, `split_toggle`, `render_header_controls`,
     `render_scope_menu`, `render_ref_selector`, `render_ref_menu`,
     `render_header_strip`, `add_color`, `del_color`, all row/body helpers from
     `notice_row` through `render_file_body_upto`, and `impl Render for Changes`.
     Keep `render_file_body_with_syntax`, `split_adder_left`, and
     `comment_adder_left` reachable at their current restricted/public paths.
6. `crates/ui/src/changes/mod.rs` — facade and ownership boundary:
   - module docs/imports needed by the facade, shared layout and fold timing
     constants,
     `DiffMode` plus `persist_split`, `ParsedDiff`, `FileFold`,
     `HighlightSlot`, `DiffHighlightState`, `RefMenu`, `HoverRow`,
     `CommentDraft`, the `Changes` fields, `ChangesEvent`, its `EventEmitter`
     implementation, and constructors (`new`, `for_commit`, `tab_title`).
   - Declare `mod model; mod rows; mod sync; mod comments; mod render;` and
     explicitly `pub use` the public model/row constants and helpers consumed
     through `crate::changes`. Use `pub(super)` for child-only helpers and
     fields; do not make implementation details `pub` merely to bypass a
     borrow or privacy error.

The manifest is a responsibility guide, not permission to change behavior.
If a type must move with a dependent helper to compile, move the smallest
verbatim unit and record that deviation in the final handoff.

### Existing conventions to follow

The repository uses module directories with a facade and focused children;
`crates/ui/src/transcript/mod.rs` + `transcript/{model,markdown,viewport,tool,render}.rs`
and `crates/ui/src/pickers.rs` + `pickers/{logic,catalog,provider_model,checkout,space,common}.rs`
are the current examples. Their facades keep public API paths via explicit
`pub use`, while child-only items use `pub(super)`. Match that pattern.

GPUI ownership rules from `docs/research/gpui.md` require long-lived
`Subscription`/`Task` values to remain fields on the owning entity, UI updates
to return through `cx.update`, and CPU parsing/highlighting to use
`background_executor()`. Do not detach the watch, scoped fetch, parse,
highlight, or fold-settle tasks while moving them. `ARCHITECTURE.md` and
`AGENTS.md` require the UI to remain behind the typed RPC boundary and the
vendored GPUI snapshot; do not alter RPC methods, engine code, or `vendor/gpui`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Drift/status | `git diff --stat 9b20de6..HEAD -- crates/ui/src/changes.rs crates/ui/src/changes crates/ui/src/lib.rs plans/README.md` | empty at the planned baseline; any output requires manifest re-check |
| UI characterization | `rtk cargo test -p holt-ui --lib` | 587 passed at plan time; after the move the same tests pass, with count changing only because test modules were relocated |
| Workspace regression | `rtk cargo test --workspace` | 967 passed, 1 ignored at plan time |
| Lint | `rtk cargo clippy --workspace` | exit 0; retain the existing warning set (including the pre-existing `transcript/render.rs:233` too-many-arguments warning) |
| Format baseline | `rtk cargo fmt -p holt-ui -- --check` | currently exits 1 only for pre-existing `composer/popups.rs` and `loaders.rs` diffs; do not fix those out-of-scope files; new `changes` files must add no formatting diff |
| Scope check | `git status --short` | only the files listed in Scope are modified |
| Move review | `git diff --color-moved=dimmed-zebra -- <in-scope paths>` | moved bodies are move-colored; non-moved hunks are only module/use/re-export/visibility wiring or relocated tests |

`cargo fmt --all` is not a valid gate in this repository because the frozen
`vendor/gpui` snapshot creates multiple workspace roots. The package command
above is the documented check; its unrelated baseline failure must remain
unchanged.

## Scope

**In scope (only these files may be modified):**

- `crates/ui/src/changes.rs` converted into the facade
  `crates/ui/src/changes/mod.rs` (the old flat file is removed only after the
  directory module compiles)
- New children under `crates/ui/src/changes/`: `model.rs`, `rows.rs`,
  `sync.rs`, `comments.rs`, and `render.rs`
- `plans/README.md` status row for plan 006

**Out of scope (do not touch):**

- `crates/ui/src/lib.rs`, `shell.rs`, `shell/right_pane.rs`,
  `transcript/`, `comments.rs`, `history.rs`, `state.rs`, `settings/`, RPC,
  engine, `vendor/gpui`, or any other source file
- Public names, public/restricted visibility contracts, event payloads, RPC
  method names/parameters, patch parsing rules, row IDs, layout constants,
  rendered strings, animation durations, list anchoring, or async scheduling
- New features, visual redesign, performance tuning, warning cleanup, or
  changing the existing tests beyond physically relocating them with their
  implementation

## Git workflow

- Use a branch named `refactor/006-split-changes` (or the repository's
  equivalent `codex/` prefix if branch creation is managed externally).
- Follow the observed commit style, for example:
  `refactor(ui): split changes model and rendering modules`.
- Prefer one commit per extraction step. Do not push or merge unless the
  operator explicitly asks.

## Steps

### Step 1: Establish the baseline and create the facade

Run the drift check and all commands in the table; record the actual test
counts and warning/format baselines. Create `crates/ui/src/changes/` and move
the module docs, imports, shared constants, `DiffMode`/persistence, `ParsedDiff`,
the `Changes` struct, `ChangesEvent`, constructors, and the module-level
`EventEmitter` implementation into `changes/mod.rs`. Create empty child files
for the five declarations, but initially keep the remaining implementation in
the facade so the directory form compiles before extraction. Preserve
`pub mod changes;` in `crates/ui/src/lib.rs` and do not leave both `changes.rs`
and `changes/mod.rs` active.

**Verify**: `rtk cargo check -p holt-ui` exits 0 and
`rtk cargo test -p holt-ui --lib` still reports 587 passed (or the recorded
baseline if the repository drifted before the STOP check).

### Step 2: Extract pure model and row modules

Move the symbols in manifest sections 1 and 2 verbatim into `model.rs` and
`rows.rs`. Keep model tests with parser/scope/highlight helpers and row tests
with flattening, split pairing, sticky headers, analytic heights, and gutters.
Duplicate only small test fixtures when Rust's child-module privacy requires
it, following the existing per-child `mod tests` convention. Add explicit
facade re-exports for all existing public names, including `FileDiff`,
`DiffLine`, `DiffHighlights`, `DiffScope`, `DiffRow`, `body_height`,
`truncate_file_lines`, `DIFF_LINE_HEIGHT`, and the comment-adder constants.

Do not rewrite expressions, parse grammar, row IDs, split pairing, comments,
or assertions. Use `pub(super)` for child-to-facade access and retain `pub` at
the facade boundary where external transcript/shell callers currently rely on
it.

**Verify**: `rtk cargo test -p holt-ui --lib` passes the recorded count;
`git diff --check` reports no whitespace errors; `rtk cargo clippy --workspace`
exits 0 with only the recorded warnings.

### Step 3: Extract synchronization and comment entity methods

Move the `sync.rs` and `comments.rs` items from manifest sections 3 and 4 as
`impl Changes` blocks. The `Changes` fields remain defined once in `mod.rs`;
child impls may access private facade fields because they are sibling modules
of the same parent, and visibility should be widened only when the compiler
requires it. Preserve watch retry timing and error text, scope/fetch keys,
late-result guards, list splices/resets, fold settle timing, composer-key
guards, and all `cx.notify()` calls exactly.

Do not split `Changes` into another entity or introduce a state abstraction.
Do not move a `Task` or `Subscription` into a detached global. If a cyclic
dependency appears between `rows` and `sync`, keep the pure row functions in
`rows` and import them from `super`, rather than duplicating logic.

**Verify**: `rtk cargo check -p holt-ui` exits 0; `rtk cargo test -p holt-ui
--lib` passes the baseline; `rtk cargo test --workspace` reports 967 passed
and 1 ignored (or the Step 1 recorded counts).

### Step 4: Extract rendering and finalize the compatibility facade

Move the GPUI-only symbols in manifest section 5 into `render.rs`, including
the `Render` implementation and all header/menu/body helpers. Keep the
`HighlightSlot` task fields owned by `Changes`; preserve lazy excerpt/full
highlight behavior and the stale-source checks. Keep `render_row` using the
same `DiffRow` variants and `ListState` indices, and keep the file-body helper
usable by transcript tool details without changing its signature.

Finish `mod.rs` with explicit `pub use` lines matching every current external
consumer. Remove imports made obsolete by the moves, then remove the old flat
`changes.rs` only after the directory module is the sole module entry. Review
the staged diff for accidental formatting or semantic edits; a pure extraction
must be almost entirely move-colored.

**Verify**: `rtk cargo test --workspace` exits 0 with the recorded counts;
`rtk cargo clippy --workspace` exits 0 with baseline warnings only; the
package format check has no new `changes`-path diff; `git diff --color-moved`
shows only allowed wiring/visibility/test-relocation hunks; and
`git status --short` contains only Scope paths.

## Test plan

- Relocate, without rewriting, all existing `#[cfg(test)]` tests. Model tests
  cover patch parsing, quoted paths, file status/notices, truncation, scope
  labels/default refs, frame folding, source matching, and highlight mapping.
  Row tests cover unified/split flattening, pairing/no-newline markers,
  sticky-header positioning/theme, comment anchors, analytic heights, and
  gutter sizing.
- Keep tests that need private `Changes` entity state in `mod.rs` or
  `render.rs`; do not create a GPUI test harness or modify Cargo features.
- Use the existing `crates/ui/src/transcript/tool.rs` tests as the compatibility
  oracle for `FileDiff`/`body_height`/`render_file_body_with_syntax` consumers.
- Verification is the existing suite, not new behavior coverage:
  `rtk cargo test -p holt-ui --lib` and `rtk cargo test --workspace` must pass
  with no unexplained count change. A count change is acceptable only when the
  executor names the physically relocated test module in the handoff.

## Done criteria

- [ ] `crates/ui/src/changes/mod.rs` is the sole Changes module entry; the old
  flat `changes.rs` is gone and no duplicate module is active.
- [ ] `model.rs`, `rows.rs`, `sync.rs`, `comments.rs`, and `render.rs` contain
  only the responsibilities in the move manifest, with no duplicated logic.
- [ ] Existing `crate::changes::{...}` call sites compile unchanged, including
  transcript diff rendering and shell surface tabs.
- [ ] Preparing/clean/list states, watch errors, scope/base-ref behavior,
  folds/list splices, comments, highlights, unified/split rendering, and
  `ChangesEvent` payloads are behaviorally unchanged.
- [ ] `rtk cargo test -p holt-ui --lib` and `rtk cargo test --workspace` pass
  with the recorded baseline counts; `rtk cargo clippy --workspace` exits 0.
- [ ] The known package-format baseline remains limited to the unrelated files
  listed in Commands; no new format diff exists under `crates/ui/src/changes`.
- [ ] `git status --short` lists only the in-scope source/module files and
  `plans/README.md`; plan 006's row is updated to DONE or BLOCKED with a
  reason.

## STOP conditions

- The drift check shows changes to any manifest symbol, or the current code no
  longer matches the line/symbol descriptions above.
- Preserving existing `crate::changes` paths would require editing an
  out-of-scope caller, changing an event/RPC/public response shape, or making
  broad implementation details `pub`.
- Moving code would require changing row IDs, patch grammar, rendered text,
  animation timing, list splice/reset behavior, task cancellation/ownership,
  or the checkout/device/cwd resolution order.
- A child module would need a second owner for `Changes` fields, a detached
  watch/highlight/fetch task, or a new cross-module state abstraction.
- Any verification command fails twice after a reasonable import/visibility
  correction, or the test count changes for a reason other than test
  relocation.
- Formatting would require touching the known unrelated baseline files or any
  other out-of-scope path.

## Maintenance notes

- Place future Changes work in the child matching its primary responsibility;
  keep `mod.rs` focused on shared constants, entity fields, constructors,
  events, and compatibility re-exports.
- Reviewers should inspect move-colored diffs, public-path preservation, task
  and subscription ownership, `ListState::splice/reset` range arithmetic, and
  the fact that split mode still only re-flattens the shared parsed model.
- Syntax highlighting remains a Changes-owned cache/task concern and is not
  redesigned here. A later extraction into a shared service needs a separate
  plan because transcript rendering depends on the current helper signature.
- The existing package-format failures in `composer/popups.rs`, `loaders.rs`,
  and `proto/motion.rs` are baseline debt; do not silently absorb them into
  this refactor.
