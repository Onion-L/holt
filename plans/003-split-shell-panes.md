# Plan 003: Extract `shell/titlebar.rs`, `shell/chat_list.rs`, `shell/right_pane.rs`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in "STOP conditions" occurs, stop and report — do
> not improvise. When done, update your row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 5c2f3f4..HEAD -- crates/ui/src/shell.rs crates/ui/src/shell/spaces.rs crates/ui/src/shell/tabs.rs`
> — written against `5c2f3f4`, where shell.rs is exactly 6014 lines. On any
> difference or excerpt mismatch, STOP.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: LOW (all three steps are pure code movement; the wiring trick
  is pre-proven by `shell/spaces.rs` and `shell/tabs.rs`)
- **Depends on**: none (parallelizable with plans 001/002 — disjoint files)
- **Category**: tech-debt
- **Planned at**: commit `5c2f3f4`, 2026-09-01

## Why this matters

`crates/ui/src/shell.rs` is 6014 lines with a ~4300-line `impl Shell`, a
404-line `Render::render`, a 396-line `render_right_tab_strip`, and a
377-line `render_chat_row`. Three regions are self-contained by concern —
titlebar chrome, the sidebar chat list, and right-pane surface management —
and the module ALREADY has the split convention: `shell/spaces.rs:201` and
`shell/tabs.rs:42` open their own `impl Shell` blocks off `Shell`'s private
state (spaces.rs's header says so explicitly). This plan extracts the three
regions along those existing seams (~2500 lines out), leaving boot/orchestration,
overlays, main column, and `render()` in shell.rs.

## Current state

`crates/ui/src/shell/` contains `spaces.rs` (2487) and `tabs.rs` (410).
The wiring that makes this work (shell.rs:50–53):

```rust
mod spaces;
mod tabs;

use spaces::{AddSpaceFlow, RenameSpaceDialog};
```

Children open with `use super::*;` (spaces.rs:10, tabs.rs:7), which imports
the parent's entire namespace — including shell.rs's private `use` imports —
so children never duplicate the parent's import block. `Shell` (shell.rs:703,
~90 private fields) stays in shell.rs; children add `impl Shell` blocks and
call sibling methods freely in both directions.

Verified facts that make the three cuts safe (all grepped at `5c2f3f4`):

- Nothing outside the `shell` module subtree references any item being moved
  (`titlebar_*`/`cluster_*`/`caption_*` geometry, `ChatMenuState`,
  `RightSurface`/`SessionPanels`, `RESORT`, etc.). `tabs.rs` uses
  `TITLEBAR_ACTION_SLOT_WIDTH`, `TITLEBAR_IDENTITY_GAP`,
  `TITLEBAR_ACTION_EDGE_INSET`, `titlebar_plus_alpha`,
  `title_bar_content_start`, `titlebar_right_pad` — all stay reachable via
  its existing `use super::*` glob once shell.rs re-imports them (below).
- `render()` calls the moved regions only as methods
  (`render_title_bar` shell.rs:5500, `render_sidebar` :5411,
  `render_right_pane` :5442) — method calls need no imports.
- The `Render` impl (5195–5599), `on_state_changed` (1073–1332), overlays
  (3490–3709), settings I/O (1891–2105), chat actions (1908–2261), pane
  geometry methods (1333–1438, 2262–2348), main-column rendering
  (3776–4228), and the nav history stay in shell.rs.

### Move manifests

**Cut A — `shell/titlebar.rs`** (shell.rs → titlebar.rs):

| Kind | Items (current shell.rs lines) |
|---|---|
| free fn | `titlebar_new_session_alpha` (139–149, stays private), `titlebar_cluster_start` (161), `titlebar_spacer_width` (168), `caption_buttons_width` (199), `cluster_buttons_start` (210), `cluster_clearance` (222) — keep their existing `pub` |
| consts | `TITLEBAR_CONTROL_GAP` (176), `TITLEBAR_GROUP_GAP` (179), `TITLEBAR_IDENTITY_GAP` (181), `TITLEBAR_ACTION_EDGE_INSET` (184), `CLUSTER_BUTTONS_WIDTH` (187), `TITLEBAR_ACTION_SLOT_WIDTH` (190), `TITLEBAR_CLUSTER_PAD` (195, private), `WINDOWS_CAPTION_BUTTON_WIDTH`/`WINDOWS_CAPTION_WIDTH` (5021–5022, private) |
| `impl Shell` methods | `titlebar_spacer` (2349), `title_bar_content_start` (2370), `render_title_bar` (2383), `titlebar_drag_region` (2405), `render_titlebar_cluster` (2461), `titlebar_plus_alpha` (2536), `render_windows_caption_controls` (2547), `resolve_linux_captions` (2603 macOS + 2639 other, BOTH cfg arms), `linux_left_caption_count` (2643), `linux_right_caption_count` (2648), `titlebar_right_pad` (2655), `render_linux_caption_controls` (2666) |
| free render helpers | `grid_backdrop` (4888), `window_control_button` (4976), `titlebar_right_padding` (5027), `windows_caption_button` (5039), `linux_caption_button` (5085), `nav_history_button` (5130), `header_icon_button` (5159) — all private, used only within this file |
| tests | `new_session_action_lives_in_the_titlebar_only_when_useful` (5645), `titlebar_cluster_matches_holt_window_controls` (5683), `titlebar_spacer_selects_per_platform_and_fullscreen` (5698), `windows_caption_controls_reserve_titlebar_space` (5718), `linux_caption_controls_reserve_titlebar_space` (5724), `cluster_clearance_clears_the_overlay_buttons` (5744) |

**Cut B — `shell/chat_list.rs`** (shell.rs → chat_list.rs):

| Kind | Items (current shell.rs lines) |
|---|---|
| types | `ChatMenuPage` (70), `ChatMenuState` (76) — become `pub(super)` (used by shell.rs's `render_overlays` and by spaces.rs:1073) |
| free fns/consts | `RESORT` (517, keep `pub`), `resort_offsets` (523, keep `pub`), `sidebar_key_order_changed` (551, private), `chat_row_height` (565, already `pub(super)`), `SIDEBAR_LIST_GAP` (580), `SIDEBAR_ACTIVE_HARNESS_*` (584–585), `SIDEBAR_ARCHIVED_HARNESS_*` (586–587), `SIDEBAR_GLASS_FADE_BAND` (591) — the consts become `pub(super)` (spaces.rs uses `SIDEBAR_LIST_GAP` via `use super::*`) |
| `impl Shell` methods | `render_sidebar` (2727), `render_settings_nav` (2755), `render_chat_row` (2864), `render_connection_pill` (3241), `render_chat_sidebar` (3290), `render_sidebar_settings_row` (3463) |
| tests | the `keys` helper (5830) + `sidebar_chat_height_tracks_visible_metadata`, `sidebar_provider_geometry_reflects_row_hierarchy`, `sidebar_height_change_is_not_a_reorder`, `resort_offsets_empty_when_order_unchanged`, `resort_offsets_activity_moves_row_to_top`, `resort_offsets_respect_heights_and_gap`, `resort_offsets_ignore_added_and_removed_keys`, `resort_glide_spec_matches_original` (5834–5908) |

Plus, in the same step: `SidebarDisclosureMotion` (86–119) moves INTO
`shell/spaces.rs` (its companion methods `begin_sidebar_disclosure_motion`
etc. already live there; shell.rs field at :726 and spaces.rs:211–216 are its
only users), along with its test
`sidebar_disclosure_motion_lands_exactly_on_its_target` (6007–6013).

**Cut C — `shell/right_pane.rs`** (shell.rs → right_pane.rs):

| Kind | Items (current shell.rs lines) |
|---|---|
| types | `right_pane_max_width` (358), `right_pane_takeover_width` (364) — become `pub(super)` (shell.rs's `right_target` :1381 and two tests use them); `RightSurface` (373), `ChatPanels` (392), `SessionPanels` (404) — keep `pub` |
| drag types | `RightPaneResize` (596), `RightTabDrag` (599), `RightTabDragState` (608), `SurfaceTabGhost` (616) + its `impl Render` (620–639) — `RightPaneResize`/`RightTabDrag`/`RightTabDragState` become `pub(super)` (shell.rs's `render`/fields/on_right_pane_drag use them) |
| `impl Shell` methods | `right_terminal_panel` (1439), `right_surface_rows` (1451), `reorder_right_tabs` (1486), `update_right_tab_drag_over` (1501), `resolved_right_active` (1525), `set_right_active` (1541), `add_diff_surface` (1565), `add_commit_diff_surface` (1572), `register_diff_surface` (1581), `add_terminal_surface` (1601), `on_transcript_event` (1619), `add_subagent_surface` (1647), `spawn_subagent_snapshot_fetch` (1703), `close_right_surface` (1742), `render_right_pane` (4229), `render_surface_picker` (4348), `close_right_plus` (4412), `render_right_tab_strip` (4422 — keep its `pub(crate)`), `toggle_right_pane_expand` (4818) |
| tests | `right_pane_ceiling_preserves_the_chat_floor` (5618), `right_pane_takeover_consumes_the_chat_column` (5628), `right_pane_takeover_control_reverses_direction` (5634), `session_panels_default_closed_per_chat` (5766), `session_panels_flags_are_chat_scoped` (5779), `session_panels_both_flags_coexist_per_chat` (5799), `session_panels_update_tracks_right_surfaces` (5818) |

Stays in shell.rs: `JumpSession` (151) and `apply_keymap` (236) (keybinding
facade, called from `lib.rs`), `SettingsSection`/`Route`/`NavEntry`/
`NavHistory` (317–515), pane geometry (121–135, 1333–1438, 2262–2348),
`SidebarResize`/`TerminalResize`/`DragGhost`/`WidthTween`/`SplashPhase`/
`RenameChatDialog`/`SubagentTab` (594–702), `impl Shell` core, overlays,
settings I/O, chat actions, main column, `impl Render`, and tests:
`every_default_shortcut_binds_on_this_platform`, `pane_resize_hitboxes_yield_the_titlebar_chrome`,
`right_panel_content_keeps_the_larger_width_only_during_transition`, the six
`nav_*` tests + `chat` helper.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt -p holt-ui -- --check` | exit 0, no output |
| Format fix | `cargo fmt -p holt-ui` | exit 0 |
| Lint | `cargo clippy --workspace` | exit 0 (baseline warnings only) |
| Tests | `cargo test -p holt-ui` | `test result: ok. 525 passed; 0 failed` |

**Do not run `cargo fmt --all`** — broken by the vendored gpui workspace
(plans/README.md).

## Scope

**In scope**:
- `crates/ui/src/shell.rs` (modify)
- `crates/ui/src/shell/titlebar.rs` (create)
- `crates/ui/src/shell/chat_list.rs` (create)
- `crates/ui/src/shell/right_pane.rs` (create)
- `crates/ui/src/shell/spaces.rs` (modify — receives `SidebarDisclosureMotion`
  + one test; nothing else)

**Out of scope**:
- `crates/ui/src/shell/tabs.rs` — compiles unchanged (its `use super::*`
  picks the moved titlebar items back up through shell.rs's re-imports).
- Any further split of shell.rs (overlays, settings outlet, chat actions are
  deliberately left for a future pass).
- `plans/001` files (`settings/appearance*`) and `plans/002` files
  (`engine/*`) — different branches.
- Any behavior change, rename, or signature change. The giant functions
  (`render_right_tab_strip` 396 lines, `render_chat_row` 377 lines) move
  **verbatim** — shrinking them is a separate future task.

## Git workflow

- Worktree `../holt-w003`, branch `refactor/003-split-shell`
  (`git worktree add ../holt-w003 -b refactor/003-split-shell 5c2f3f4`;
  `export CARGO_TARGET_DIR=/Users/onion/workbench/holt/target`).
- One commit per step:
  - C1 `refactor(ui): extract titlebar chrome into shell/titlebar`
  - C2 `refactor(ui): extract the sidebar chat list into shell/chat_list`
  - C3 `refactor(ui): extract right-pane surface management into shell/right_pane`
- **Before every commit**, run the review gate in plans/README.md.

## Steps

The same wiring is added once and extended per step. After the final step
shell.rs lines 50–53 read:

```rust
mod chat_list;
mod right_pane;
mod spaces;
mod tabs;
mod titlebar;

use chat_list::*;
use right_pane::*;
use titlebar::*;
use spaces::{AddSpaceFlow, RenameSpaceDialog, SidebarDisclosureMotion};
```

Why this works: `use chat_list::*;` privately re-imports the moved names
into the `shell` namespace, so (a) shell.rs's own code keeps using them
unqualified, (b) `tabs.rs`/`spaces.rs` keep getting them through their
existing `use super::*;`, and (c) shell.rs's `mod tests` (`use super::*`)
sees everything. The three children export disjoint names (verified), so
the globs cannot collide.

**Visibility rule for moved `impl Shell` methods — apply uniformly**: a
private method defined in a child module is NOT visible to shell.rs (Rust
visibility is module-scoped, parent cannot see child's private items), and
many moved methods ARE called from shell.rs code that stays (`render()`
calls `render_title_bar` :5500, `render_sidebar` :5411, `render_right_pane`
:5442; `new` calls `Self::on_transcript_event` :913; `on_state_changed`
and the action handlers call `resolved_right_active` :1303,
`set_right_active`, `add_*_surface`, `close_right_surface`). Therefore:
**mark every moved `impl Shell` method `pub(super)`** — including ones that
happen to be called only within their new file (harmless: they are all used
somwhere, so no dead-code warning). The ONLY exception: `render_right_tab_strip`
keeps its existing `pub(crate)` (wider than `pub(super)`, and tabs.rs:226
calls it). Moved free functions/types follow the per-item visibility listed
in the manifests.

### Step 1: `shell/titlebar.rs` → commit C1

1. Create `crates/ui/src/shell/titlebar.rs`:
   - Header: `//! Titlebar chrome: geometry constants, cluster/caption
     rendering, and the platform caption buttons. Child module of `shell`
     so it renders straight off `Shell`'s private state.`
   - Then `use super::*;`
   - Move every item in manifest Cut A verbatim. Visibility: all `impl
     Shell` methods become `pub(super)` per the uniform rule above; the
     `pub` free fns/consts keep `pub`; `titlebar_new_session_alpha`,
     `TITLEBAR_CLUSTER_PAD`, `WINDOWS_CAPTION_*` and the free render
     helpers stay private (used only within this file).
   - Add `#[cfg(test)] mod tests { use super::*; … }` with the 6 listed
     tests moved verbatim.
2. In shell.rs: add `mod titlebar;` + `use titlebar::*;` (keep the mod list
   alphabetical: `mod spaces; mod tabs; mod titlebar;`). Delete the moved
   source lines.

**Verify**: `cargo fmt -p holt-ui -- --check` (run `cargo fmt -p holt-ui`
to format the new file first) → `cargo clippy --workspace` →
`cargo test -p holt-ui` → **525 passed** (31 shell tests still 31: 6 moved,
25 stay). Review gate, commit C1.

### Step 2: `shell/chat_list.rs` + `SidebarDisclosureMotion` → spaces.rs → commit C2

1. Create `crates/ui/src/shell/chat_list.rs`:
   - Header: `//! The sidebar's session list: chat rows, resort glide,
     settings nav, and the chat context menu. Child module of `shell` so it
     renders straight off `Shell`'s private state.`
   - `use super::*;`
   - Move every item in manifest Cut B. Visibility: `ChatMenuPage`/
     `ChatMenuState` and the `SIDEBAR_*` consts become `pub(super)`;
     `RESORT`/`resort_offsets` keep `pub`; `chat_row_height` keeps
     `pub(super)`; `sidebar_key_order_changed` stays private; all six
     methods become `pub(super)` per the uniform rule.
   - Tests: the `keys` helper + the 8 listed tests move verbatim. If any
     test left behind in shell.rs also used `keys` (compiler error E0425),
     STOP and report — do not duplicate it.
2. Edit `shell/spaces.rs`: paste `SidebarDisclosureMotion` (86–119, keep
   `pub(super)`) and its impl near the top of the file after its imports,
   and append its test to spaces.rs's existing `#[cfg(test)]` mod
   (spaces.rs:2454, which imports `use super::compare_sidebar_chats;` —
   extend to `use super::{compare_sidebar_chats, SidebarDisclosureMotion};`
   or rely on the file-level namespace since the type now lives there).
3. In shell.rs: add `mod chat_list;` + `use chat_list::*;`, change the
   spaces import to `use spaces::{AddSpaceFlow, RenameSpaceDialog, SidebarDisclosureMotion};`,
   delete moved source (including 86–119).

**Verify**: fmt → clippy → `cargo test -p holt-ui` → **525 passed**.
Review gate, commit C2. (Also sanity-check `grep -n "SidebarDisclosureMotion" crates/ui/src/shell.rs`
shows exactly one hit — the `use` line.)

### Step 3: `shell/right_pane.rs` → commit C3

1. Create `crates/ui/src/shell/right_pane.rs`:
   - Header: `//! The right pane: surface tabs (diffs, terminals, subagent
     transcripts), drag-reorder, and the surface picker. Child module of
     `shell` so it renders straight off `Shell`'s private state.`
   - `use super::*;`
   - Move every item in manifest Cut C. Visibility: all `impl Shell`
     methods become `pub(super)` per the uniform rule, EXCEPT
     `render_right_tab_strip` which keeps `pub(crate)` (tabs.rs:226);
     `RightPaneResize`, `RightTabDrag`, `RightTabDragState` become
     `pub(super)`; `right_pane_max_width`/`right_pane_takeover_width`
     become `pub(super)` (per the manifest); `SurfaceTabGhost` stays
     private; `RightSurface`/`ChatPanels`/`SessionPanels` keep `pub`
     (fields included).
   - Tests: the 7 listed tests move verbatim.
2. In shell.rs: add `mod right_pane;` + `use right_pane::*;`, delete moved
   source.

**Verify**: fmt → clippy → `cargo test -p holt-ui` → **525 passed**.
`wc -l crates/ui/src/shell.rs` → expect ~3400–3600. Review gate, commit C3.

## Test plan

No new tests. All 31 shell.rs tests survive verbatim: 6 → titlebar.rs,
9 items (helper + 8 tests) → chat_list.rs, 1 → spaces.rs, 7 →
right_pane.rs, 8 stay (incl. the `chat` helper). Gate: 525 passed at every
commit; `grep -c "#\[test\]" crates/ui/src/shell.rs crates/ui/src/shell/*.rs`
totals 31 across the shell subtree.

## Done criteria

- [ ] `cargo fmt -p holt-ui -- --check` exits 0
- [ ] `cargo clippy --workspace` exits 0 (baseline warnings only)
- [ ] `cargo test -p holt-ui` → 525 passed, 0 failed
- [ ] `ls crates/ui/src/shell/` shows `titlebar.rs`, `chat_list.rs`,
      `right_pane.rs` alongside `spaces.rs`, `tabs.rs`
- [ ] `wc -l crates/ui/src/shell.rs` reports ≤ 3700
- [ ] `git diff 5c2f3f4..HEAD -- crates/ui/src/shell/tabs.rs` is EMPTY
      (tabs.rs untouched) and spaces.rs's diff contains only the
      `SidebarDisclosureMotion` addition + one test
- [ ] `git status --short` shows no files outside the In-scope list
- [ ] 3 commits on `refactor/003-split-shell`, each review-gated
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check fails, or any manifest line range doesn't match the live
  code.
- A moved item fails to compile for a reason other than a missing `use`
  import or a manifest-listed visibility change.
- An item you are moving turns out to be referenced from OUTSIDE
  `crates/ui/src/shell/` (grep first if unsure) — the plan's premise is
  that nothing external moves; if false, STOP rather than adding re-exports
  to the crate.
- The 525-test count changes, or any test needs editing.
- The three glob re-imports (`use chat_list::*` etc.) produce an ambiguity
  error — means two children export the same name (the plan verified they
  don't at `5c2f3f4`); report the colliding names instead of renaming.
- The reviewer rejects a commit twice.

## Maintenance notes

- shell.rs still holds ~3500 lines after this plan; the next natural cuts
  (in priority order) are `shell/overlays.rs` (render_overlays +
  rename/delete confirms, 3490–3709), `shell/settings.rs`
  (open/close/outlet, 1891–2105), and `shell/chat_actions.rs`
  (1908–2261). Do them in a separate pass with the same pattern.
- `render_right_tab_strip` (396 lines) and `render_chat_row` (377 lines)
  moved verbatim on purpose; if someone shrinks them later, the row-chip
  and drag-ghost builders are the natural sub-extractions.
- `tabs.rs` and `spaces.rs` depend on shell.rs's private re-import globs;
  if a future change renames a moved item, remember its `use super::*`
  consumers — the compiler will catch it, but reviewers should expect
  cross-file fallout there.
