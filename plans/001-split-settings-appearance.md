# Plan 001: Split `settings/appearance.rs` into a module directory

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in "STOP conditions" occurs, stop and report — do
> not improvise. When done, update your row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 5c2f3f4..HEAD -- crates/ui/src/settings/appearance.rs`
> — this plan was written against `5c2f3f4`, where the file is exactly 2540
> lines. If the file differs (or the excerpts below don't match), STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW–MED (commits 1–3 are pure code movement; commit 4 contains
  one small, fully specified extraction)
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `5c2f3f4`, 2026-09-01

## Why this matters

`crates/ui/src/settings/appearance.rs` is 2540 lines holding six independent
concerns: font/size pickers, the per-mode theme selector, accent/surface
controls, the VS Code theme import dialog (a 512-line function), the theme
library review rows, and the preview miniatures. Its only consumer is
`shell.rs` (three references: shell.rs:33, :764, :2027), so the split is
externally invisible if the `crate::settings::appearance::AppearancePage`
path is preserved. The repo already did this exact refactor to `composer.rs`
in commit `e76c903`; this plan follows the same convention.

## Current state

- `crates/ui/src/settings.rs:17` declares `pub mod appearance;` — **do not
  change this file at all**. A `foo.rs` + `foo/` directory pair satisfies the
  same `pub mod` declaration, so `settings.rs` keeps compiling untouched.
- `crates/ui/src/settings/appearance.rs` — the whole page today. Layout at
  `5c2f3f4`:

| Lines | Content |
|---|---|
| 26–36 | `struct ImportDialog` (private) |
| 38–71 | `pub struct AppearancePage` (13 private fields) + `new` |
| 73–251 | `impl AppearancePage`: 11 font/size methods (`commit_font`, `commit_size`, `close_font_menu`, `close_size_menu`, `dismiss_font_menu`, `dismiss_size_menu`, `toggle_font_menu`, `toggle_size_menu`, `on_font_key_down`, `on_size_key_down`) |
| 253–388 | import flow methods: `open_import`, `compile_import`, `choose_import_source`, `finish_import` |
| 391–423 | free fns `source_name`, `slug` |
| 425–463 | free fns `step_font`, `first_available`, `last_available` |
| 465–471 | free fn `bar` |
| 473–545 | free fns `accent_helper`, `surface_label`, `surface_helper`, `surface_choice` |
| 547–603 | free fns `miniature`, `scene_row` |
| 605–752 | free fns `mode_scene`, `mode_preview`, `model_appearance` |
| 754–787 | free fns `palette_preview`, `compact_action` |
| 789–949 | free fns `import_scene_preview`, `report_panel` |
| 951–1021 | free fn `accent_swatch` |
| 1023–1214 | `impl AppearancePage`: `theme_menu`, `theme_menu_mut`, `close_theme_menu`, `render_theme_selector` |
| 1216–1727 | `render_import_dialog` (512 lines) |
| 1729–1986 | `render_review_dialog`, `render_library_entry`, `render_theme_library_rows` |
| 1989–2454 | `impl Render for AppearancePage` — `render()` (465 lines) |
| 2456–2540 | `mod tests` — 7 tests |

- Excerpt — the struct (stays in the facade):

```rust
// appearance.rs:38
pub struct AppearancePage {
    selected_font: UiFontFamily,
    selected_size: UiFontSize,
    font_focus: FocusHandle,
    size_focus: FocusHandle,
    font_menu: Popup<()>,
    size_menu: Popup<()>,
    font_menu_dismissed_at: Option<std::time::Instant>,
    size_menu_dismissed_at: Option<std::time::Instant>,
    light_theme_menu: Popup<()>,
    dark_theme_menu: Popup<()>,
    import_dialog: Option<ImportDialog>,
    review_entry: Option<String>,
    library_error: Option<SharedString>,
}
```

- The convention to follow (from `crates/ui/src/composer.rs:12–28`):

```rust
mod input;
mod input_element;
mod layout;
// …
pub use input::*;
pub use layout::*;
pub use mentions::{SentMentionSpan, sent_mention_display};
```

  Child files open with `//! <one-line role>` then `use super::{…};` (see
  `composer/send.rs:1–15`: `use super::{Composer, ComposerEvent};` followed by
  an `impl Composer` block). Child modules can read the parent struct's
  private fields — that is the load-bearing trick this split relies on.
  Difference from composer: here the children are **private** `mod`s and the
  facade uses **private** `use child::*;` re-imports, because nothing outside
  `settings::appearance` consumes any child item (verified). Keep moved
  items' existing `pub`/private keywords unchanged; add `pub(super)` ONLY
  where a moved item is called from another file (listed per step below).

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt -p holt-ui -- --check` | exit 0, no output |
| Format fix | `cargo fmt -p holt-ui` | exit 0 (only new files may change) |
| Lint | `cargo clippy --workspace` | exit 0 (baseline dep warnings only) |
| Tests | `cargo test -p holt-ui` | `test result: ok. 525 passed; 0 failed` |

**Do not run `cargo fmt --all`** — it is broken in this repo (vendored
`vendor/gpui` workspace; see plans/README.md). Always `-p holt-ui`.

## Scope

**In scope** (the only files you may modify/create):
- `crates/ui/src/settings/appearance.rs` (modify — becomes the facade)
- `crates/ui/src/settings/appearance/fonts.rs` (create)
- `crates/ui/src/settings/appearance/previews.rs` (create)
- `crates/ui/src/settings/appearance/theme_selector.rs` (create)
- `crates/ui/src/settings/appearance/import.rs` (create)
- `crates/ui/src/settings/appearance/library.rs` (create)

**Out of scope** (do NOT touch):
- `crates/ui/src/settings.rs` — the `pub mod appearance;` declaration already
  covers the directory layout.
- `crates/ui/src/shell.rs` — its `use crate::settings::appearance::AppearancePage`
  keeps working because the struct stays in the facade.
- Any behavior change, rename, comment rewrite (beyond the new module doc
  headers), or the pre-existing `composer/send_mode.rs:118` unused-import
  warning.

## Git workflow

- Work in the worktree `../holt-w001` on branch
  `refactor/001-split-appearance` (setup in plans/README.md; if it doesn't
  exist: `git worktree add ../holt-w001 -b refactor/001-split-appearance 5c2f3f4`
  from the repo root, then `export CARGO_TARGET_DIR=/Users/onion/workbench/holt/target`).
- One commit per step, messages:
  - C1 `refactor(ui): split appearance fonts and previews into modules`
  - C2 `refactor(ui): move theme selector controls into their own module`
  - C3 `refactor(ui): move theme import dialog and library rows into modules`
  - C4 `refactor(ui): extract the font controls section from AppearancePage::render`
- **Before every commit**, run the review gate in plans/README.md (machine
  checks, scope check, `--color-moved=dimmed-zebra` check, independent
  reviewer with the step's move manifest).

## Steps

Each of steps 1–3 is a **pure move**: cut the listed items from
`appearance.rs`, paste verbatim into the new child file, add the child's
`//!` header + `use super::*;`, add the facade's `mod`/`use` lines, and mark
`pub(super)` exactly on the items listed. Every step leaves the tree
compiling and all 525 tests passing.

### Step 1: scaffold + `fonts.rs` + `previews.rs` → commit C1

1. Create `crates/ui/src/settings/appearance/fonts.rs`:
   - Header: `//! The interface font/size pickers: menu toggling, keyboard
     navigation, and the pure stepping helpers.`
   - Then `use super::*;`
   - Move the 11 font/size methods (appearance.rs:73–251) as an
     `impl AppearancePage { … }` block. Mark every one `pub(super) fn` —
     until Step 4 the facade's `render()` still calls
     `on_font_key_down`, `toggle_font_menu`, `dismiss_font_menu`,
     `commit_font`, `on_size_key_down`, `toggle_size_menu`,
     `dismiss_size_menu`, `commit_size`.
   - Move free fns `step_font`, `first_available`, `last_available`
     (425–463) — keep private (only used inside this file).
   - Move 3 tests (2503–2539): `font_options_appear_once_in_stable_order`,
     `font_keyboard_navigation_stops_at_edges_and_skips_unavailable`,
     `font_size_options_are_ordered_and_include_the_default`, as
     `#[cfg(test)] mod tests { use super::*; … }`.
2. Create `crates/ui/src/settings/appearance/previews.rs`:
   - Header: `//! Theme preview miniatures: mode scenes, palette chips, and
     the bar skeleton they are painted from.`
   - `use super::*;`
   - Move free fns `bar` (465–471), `miniature` (547–594), `scene_row`
     (596–603), `mode_scene` (605–712), `mode_preview` (714–745),
     `palette_preview` (754–767) — all `pub(super) fn` (used by the facade
     and sibling modules).
3. In `appearance.rs` add after the import block:

```rust
mod fonts;
mod previews;

use previews::*;
```

   (`use fonts::*;` is NOT needed — the facade calls no fonts free fn.)
4. Delete the moved source from `appearance.rs`. If rustc/clippy reports a
   now-unused import in the facade, remove that import (expected at this
   step: none of `HashSet`/`Path`/`PathBuf` leave yet).

**Verify**: `cargo fmt -p holt-ui -- --check` (format the new files with
`cargo fmt -p holt-ui` if needed) → `cargo clippy --workspace` →
`cargo test -p holt-ui` → **525 passed**. Then review gate, commit C1.

### Step 2: `theme_selector.rs` → commit C2

1. Create `crates/ui/src/settings/appearance/theme_selector.rs`:
   - Header: `//! Per-appearance theme selection and the accent/surface
     preference controls.`
   - `use super::*;`
   - Move free fns `accent_helper` (473–483), `surface_label` (485–491),
     `surface_helper` (493–505), `surface_choice` (507–545), `accent_swatch`
     (951–1021) — all `pub(super) fn` (the facade's `render()` calls them).
   - Move the `impl AppearancePage` block 1023–1046 (`theme_menu`,
     `theme_menu_mut`, `close_theme_menu`) + `render_theme_selector`
     (1048–1214). Mark all five `pub(super) fn` (uniform rule; only
     `render_theme_selector` is called from the facade, the rest are
     internal to this file).
   - Move 2 tests (2481–2501): `accent_helper_explains_default_and_override_scope`,
     `surface_helper_explains_theme_default_and_global_overrides`.
2. In `appearance.rs` add `mod theme_selector;` and `use theme_selector::*;`
   alongside Step 1's declarations. Delete moved source; prune imports only
   on an "unused import" warning.

**Verify**: same three commands → 525 passed. Review gate, commit C2.

### Step 3: `import.rs` + `library.rs` → commit C3

1. Create `crates/ui/src/settings/appearance/import.rs`:
   - Header: `//! The VS Code theme import flow: source picking, compilation,
     the variant list, and the import modal.`
   - `use super::*;`
   - Move `struct ImportDialog` (26–36) — keep private.
   - Move `impl AppearancePage` methods `open_import` (253–306),
     `compile_import` (308–338), `choose_import_source` (340–365),
     `finish_import` (367–388), `render_import_dialog` (1216–1727) — all
     `pub(super) fn`.
   - Move free fns `source_name` (391–402) and `slug` (404–423) — private.
   - Move free fns `import_scene_preview` (789–886) and `report_panel`
     (888–949) — `pub(super) fn` (used by library.rs too).
2. Create `crates/ui/src/settings/appearance/library.rs`:
   - Header: `//! The custom-theme library rows and the theme-mapping review
     dialog.`
   - `use super::*;` plus `use super::import::{import_scene_preview, report_panel};`
   - Move `impl AppearancePage` methods `render_review_dialog` (1729–1778),
     `render_library_entry` (1780–1918), `render_theme_library_rows`
     (1920–1986) — all `pub(super) fn`.
3. In `appearance.rs` add `mod import; mod library;` (no `use` glob needed —
   the facade calls these only as methods). Delete moved source; expect to
   prune `std::collections::HashSet` and `std::path::{Path, PathBuf}` from
   the facade imports on warning (`ImportDialog`, `source_name`, `slug` were
   their last users). Also `holt_theme::vscode::{ImportReport, SourceCompilation}`
   moves to import.rs; prune on warning.

**Verify**: same three commands → 525 passed. Review gate, commit C3.
At this point `appearance.rs` should be ~700 lines (struct, `new`,
`model_appearance`, `compact_action`, `impl Render`, 2 tests).

### Step 4: extract the font controls from `render()` → commit C4

This is the **only non-pure-move change** in the plan. The block
appearance.rs **2176–2355** (from `let font_rows: Vec<AnyElement> =
availability` through the end of `let size_trigger = …;`) builds the font
menu/trigger and size menu/trigger. It references exactly five things from
the surrounding `render()`: `theme`, `availability`, `effective_font`,
`fixed`, and `cx`. Move it into fonts.rs as a new method, and replace the
block in `render()` with a single call:

```rust
let (font_trigger, size_trigger) =
    self.render_font_controls(&theme, &availability, &effective_font, &fixed, cx);
```

New method in `fonts.rs` (inside the existing `impl AppearancePage`):

```rust
pub(super) fn render_font_controls(
    &mut self,
    theme: &Theme,
    availability: &FontAvailability,
    effective_font: &UiFontFamily,
    fixed: &SharedString,
    cx: &mut Context<Self>,
) -> (AnyElement, AnyElement) {
    // …moved block, with the mechanical adjustments below…
    (font_trigger.into_any_element(), size_trigger.into_any_element())
}
```

Mechanical adjustments inside the moved block — these and ONLY these:
- Occurrences of `&theme` as a function argument become `theme` (the
  parameter is already `&Theme`). There are three: `popover::menu_row_nav(&theme, …)`
  (×2, at old lines 2186 and 2278) and `popover::popover_card(&theme)`
  (old line 2213).
- `fixed.clone()` (×2, old lines 2216 and 2307) stays valid — do not touch.
- `availability.choices()`, `effective_font.label()`, comparisons like
  `family == effective_font` all compile unchanged against `&`-params.
- The block ends with the two bindings `font_trigger` and `size_trigger`;
  the method's last line is
  `(font_trigger.into_any_element(), size_trigger.into_any_element())` —
  `font_trigger` is built by a chain starting `let font_trigger = div()…;`
  and stays exactly that binding; only append the two `into_any_element()`
  calls. In `render()` the destructured values are consumed at old lines
  2428–2429 via `.child(font_trigger)` / `.child(size_trigger)`, which
  accept `AnyElement` unchanged.

Then rewrite the facade module doc (appearance.rs:1–2) to a directory map,
modeled on `composer.rs:1–11`:

```rust
//! Settings → Appearance: the page assembling the mode switch, theme
//! selectors, accent/surface controls, theme library, and font pickers.
//!
//! `fonts` — interface font/size pickers and keyboard navigation.
//! `previews` — theme preview miniatures.
//! `theme_selector` — per-appearance theme menus, accent and surface.
//! `import` — the VS Code theme import flow and modal.
//! `library` — custom-theme library rows and the review dialog.
```

**Verify**: same three commands → 525 passed; `render()` no longer mentions
`font_rows`/`size_rows` (`grep -n "font_rows\|size_rows" crates/ui/src/settings/appearance.rs`
returns nothing). Review gate with EXTRA scrutiny (reviewer must confirm the
moved block is byte-identical apart from the four enumerated adjustments),
commit C4.

## Test plan

No new tests. All 7 existing tests move with their subjects (3 → fonts.rs,
2 → theme_selector.rs, 2 stay) — the plan preserves them verbatim; the gate
is the unchanged count: `cargo test -p holt-ui` → 525 passed, 0 failed.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt -p holt-ui -- --check` exits 0
- [ ] `cargo clippy --workspace` exits 0 (baseline warnings only)
- [ ] `cargo test -p holt-ui` → 525 passed, 0 failed
- [ ] `wc -l crates/ui/src/settings/appearance.rs` reports ≤ 800
- [ ] `ls crates/ui/src/settings/appearance/` lists exactly `fonts.rs`,
      `previews.rs`, `theme_selector.rs`, `import.rs`, `library.rs`
- [ ] `grep -rn "crate::settings::appearance" crates/ui/src --include="*.rs"
      | grep -v "settings/appearance"` still shows only the 3 shell.rs
      references (path unchanged)
- [ ] `git status --short` shows no files outside the In-scope list
- [ ] 4 commits on `refactor/001-split-appearance`, each review-gated
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check fails, or any excerpt above doesn't match the live code.
- A moved item fails to compile for a reason other than a missing `use`
  (fixing missing imports is allowed; anything beyond that is not).
- `render()` references more locals inside the 2176–2355 block than the five
  parameters listed in Step 4 (means the block drifted — do not guess which).
- Any test count changes from 525 / any test needs editing to pass.
- The reviewer rejects a commit twice.

## Maintenance notes

- Future settings pages should start as `settings/<page>/` directories from
  day one; this file was the last single-file settings page.
- If the import dialog grows (linked-source watching, etc.), keep changes
  inside `appearance/import.rs`; `ImportDialog` is private to it on purpose.
- Reviewers of future diffs: the children deliberately hold `pub(super)`
  methods on a parent-defined struct — that is the house pattern
  (`composer/`, `shell/`), not a layering violation.
