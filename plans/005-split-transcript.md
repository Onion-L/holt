# Plan 005: Split the transcript view into cohesive UI modules

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving on. If a
> STOP condition occurs, stop and report instead of improvising. This is a
> structural refactor: preserve behavior and public paths; do not redesign the
> transcript.
>
> **Drift check (run first)**: `git diff --stat 4128143..HEAD -- crates/ui/src/transcript.rs crates/ui/src/lib.rs plans/README.md`
> If `transcript.rs` changed since this plan was written, compare the symbols
> listed below with the live file. A mismatch is a STOP condition until the
> move manifest is re-derived.
>
> **Drift reconciled 2026-09-03 at `4746ba4`**: commits `8926678`,
> `6d06940`, `4746ba4` (the skill-invocation fold feature) added 361 lines /
> removed 59 after this plan was written. `lib.rs` is unchanged. The manifest
> below is re-derived against the live file (8,150 lines) and already folds
> the drift in, so the executor may proceed against it.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `4128143`, 2026-09-03; manifest re-derived at
  `4746ba4` (drift: the skill-invocation fold — `RowKind::SkillChip` gained a
  `content: Option<SharedString>` field that rides the row version, plus the
  items folded into the steps below)

## Why this matters

`crates/ui/src/transcript.rs` is 8,150 lines and combines at least five
independent change axes: row/data modeling, Markdown parsing, viewport state,
tool/sidecar details, and GPUI rendering. The file has also had several recent
feature commits, so unrelated changes now compete in one merge surface and
tests are difficult to locate. Splitting by responsibility will reduce merge
conflicts and make pure logic reusable/testable while keeping the existing
`crate::transcript::{Transcript, TranscriptEvent, ...}` API intact.

This plan deliberately uses a move-first approach. It must not change row IDs,
fingerprints, scroll physics, layout constants, rendered strings, event
payloads, or the behavior of streaming handoff and viewport restoration.

## Current state

- `crates/ui/src/transcript.rs` — public transcript module, `Transcript` entity,
  row model, parsing/cache helpers, scroll state, attachment/blob coordination,
  and all GPUI rendering. `Transcript` begins at line 2257; `TranscriptEvent`
  at 2417; `impl Render for Transcript` at 6205; the test module at 6355.
- `crates/ui/src/lib.rs` — declares `pub mod transcript;`; keep this path and
  its public re-exports unchanged. Rust's module-directory form will use
  `crates/ui/src/transcript/mod.rs` after the move.
- `crates/ui/src/rail.rs` and `crates/ui/src/shell.rs` — direct consumers of
  `Transcript`, `TranscriptEvent`, and the re-exported helpers
  `single_line`, `OVERDRAW_PX`, and `OWN_SEND_TOP_INSET_PX` (also used by
  `shell/tabs.rs` and `shell/spaces.rs` via `transcript::single_line`). They
  must continue compiling without call-site rewrites unless a moved item needs
  an explicit `pub(super)`/`pub(crate)` visibility adjustment.
- `crates/ui/src/composer.rs` and `crates/ui/src/composer/send.rs` — call the
  transcript's own-send hooks and rely on the existing event/API surface.

Relevant current symbols at `4746ba4` (the executor must locate their exact
live ranges):

- Pure row/tool model: `ToolItem` (line 264), `RowKind` (942, whose
  `SkillChip` variant now carries `content: Option<SharedString>`), `Row`
  (1009), `rows_for_entry` (1174), `tool_fingerprint`, `entry_fingerprint`
  (6158), `assistant_copy_text`, `format_timestamp`, `top_gap_for`,
  `diff_rows`, `part_prefix`, `thought_item`, `skill_file_display` (930).
- Pure/mostly-pure Markdown wiring: `thought_lines` (344), `parse_for_row`
  (1579), `ParseOutcome`, and the thought block-formatting helpers.
- Pure viewport state: `StickSpring` (189), `OwnTurnAnchor` (2029),
  `ViewportAnchor` (2067), `SavedViewport`, `SavedViewportCache`, and their
  helper methods.
- Tool and sidecar helpers: `ToolDetail`, `tool_detail`, `call_block`,
  `diff_to_file`, `tool_group_summary`, `chips_height`, `detail_height`,
  `blob_detail`, `format_kb`.
- Rendering/entity integration: `impl Transcript`, `render_row`,
  `render_tool_group`, the render free functions (5390–6157), the
  `render_skill_invocation`/`render_user_skill` fold renderers, attachment
  preview/load methods, `HighlightStore`, and `impl Render for Transcript`.
- Entity-owned state that stays in the facade: `CachedRows`, `FoldState`
  (1999), `toggle_skill_fold`, `BlobFetch`, and the shell-facing
  `TranscriptEvent`.

The repo convention is a module directory with a small facade and focused
children; `crates/ui/src/composer.rs` + `crates/ui/src/composer/` and
`crates/ui/src/pickers.rs` + `crates/ui/src/pickers/` are the existing
examples. Preserve their `mod` declarations and `pub use` style. The UI is
GPUI-based and agent-agnostic: continue rendering `holt_doc::MessagePart`
data, not engine events. `ARCHITECTURE.md` and `DESIGN.md` require the typed
RPC boundary and the existing calm, explicit transcript states; neither is
changed by this plan.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt -p holt-ui -p holt-engine -p holt-doc -p holt-theme -p holt-proto -p holt-rpc -p holt-syntax -p holt -- --check` | exit 0 |
| UI tests | `cargo test -p holt-ui` | all tests pass; baseline at re-derivation (`4746ba4`) is 568 UI tests — the plan-time 525 is stale, the drift added tests |
| Workspace tests | `cargo test --workspace` | exit 0 |
| Lint | `cargo clippy --workspace` | exit 0; baseline at re-derivation is one pre-existing code warning (`render_skill_invocation` too_many_arguments, arrived with the drift) plus the repository's pre-existing dependency warnings |

`cargo fmt --all` is not a valid verification command here because the frozen
`vendor/gpui` snapshot creates multiple workspace roots; use the per-package
command above.

## Scope

**In scope (only these files may be modified):**

- `crates/ui/src/transcript.rs` (converted into the facade
  `crates/ui/src/transcript/mod.rs`; the original file is removed only after
  all moved code compiles)
- New children under `crates/ui/src/transcript/`:
  `model.rs`, `markdown.rs`, `viewport.rs`, `tool.rs`, and `render.rs`
- `plans/README.md` status row for plan 005

**Out of scope:**

- Any changes to `crates/ui/src/rail.rs`, `shell.rs`, `composer/`, `state.rs`,
  Markdown parser/render implementation, `holt-doc`, RPC, engine, or vendored
  GPUI. Do not rename public transcript items or alter event payloads.
- Feature changes, visual changes, string changes, performance tuning, or
  opportunistic warning cleanup.

## Git workflow

- Use a branch named `refactor/005-split-transcript` (or the repository's
  equivalent `codex/` prefix if branch creation is managed externally).
- Follow the observed commit style, for example:
  `refactor(ui): split transcript model and viewport modules`.
- Prefer one commit per extraction step. Do not push or merge unless the
  operator explicitly asks.

## Steps

### Step 1: Establish a characterization baseline and create the module facade

Run the drift check and the commands in the table above; record the actual UI
test count and any baseline warnings. Create `crates/ui/src/transcript/` and a
`mod.rs` facade. Move the module documentation, imports needed by the facade,
`Transcript`, `TranscriptEvent`, `impl Transcript`, and `impl Render for
Transcript` into `mod.rs` initially, preserving the existing `pub mod
transcript` declaration in `lib.rs`. Do not leave both `transcript.rs` and
`transcript/mod.rs` active at the same time.

**Verify**: `cargo check -p holt-ui` → exit 0; `cargo test -p holt-ui` → the
recorded baseline count passes.

### Step 2: Extract the pure row and Markdown modules

Move the row model and row construction symbols into `model.rs`: `ToolItem`,
`RowKind`, `Row`, `UserSkill`, `format_skill_title`, `skill_file_display`,
`format_timestamp`, `rows_for_entry`, `tool_fingerprint`, `entry_fingerprint`,
`assistant_copy_text`, `top_gap_for`, `diff_rows`, `thought_item`, and only
the helper functions/types they require (`is_agent_call`, `is_agent_tool`,
`is_spawn_link`, `tool_group_collapses`, `fnv1a`, `part_prefix`). Move
`thought_lines`, its block-formatting helpers, `ParseOutcome`, and
`parse_for_row` into `markdown.rs`. Keep the existing visibility at the public
boundary (`pub` items remain reachable through `crate::transcript` via explicit
`pub use` in `mod.rs`); use `pub(super)` for child-to-facade helpers rather
than making implementation details public. Note the module dependency cycle
this implies (`model` ↔ `tool`: `rows_for_entry` builds `ToolDetail`s while
`tool_group_summary` takes `&[ToolItem]`) is fine between sibling modules.

Do not alter expressions, constants, row ID construction, fingerprints,
Markdown parse behavior, or test assertions. Relocate the associated pure unit
tests with their implementation. Add only `use super::...`/`use crate::...`
wiring required by compilation.

**Verify**: `cargo fmt -p holt-ui -- --check` → exit 0; `cargo test -p holt-ui`
→ all tests pass with no unexplained count change; `git diff --check` → no
whitespace errors.

### Step 3: Extract viewport and tool/sidecar modules

Move viewport-only types and pure helpers into `viewport.rs`:
`StickSpring`, `OwnTurnAnchor`, `ViewportAnchor`, `SavedViewport`,
`SavedViewportCache`, `RestoredViewport`, `TranscriptReplayState`,
`ViewportFinalizeToken`, `selection_scroll_step`, `should_anchor_live_stream`,
`own_turn_reservation`, `flavour_word`, `flavour_seed`, `sending_bridge`, and
`format_elapsed`. Preserve `StickSpring` and helper visibility currently used
by tests or `rail.rs`.

Move tool-detail and sidecar value logic into `tool.rs`: `ToolDetail`,
`tool_detail`, `call_block`, `diff_to_file`, `tool_group_summary`,
`chips_height`, `detail_height`, `blob_detail`, `format_kb`,
`ChipAffordance`, and related constants. Keep GPUI-specific rendering out of
this module; `render_tool_group` remains in `render.rs` or the facade and calls
the pure tool helpers through `super` re-exports.

**Verify**: `cargo test -p holt-ui` → all tests pass; `cargo clippy --workspace`
→ exit 0 with only baseline warnings; `cargo fmt -p holt-ui -- --check` → exit
0.

### Step 4: Extract GPUI rendering and finalize the facade

Move `HighlightStore` only if it can be moved with its existing GPUI task
types; otherwise leave it in `mod.rs` and document that it is entity-owned.
Move `render_row`, `render_tool_group`, `render_skill_invocation`,
`render_user_skill`, attachment preview/load rendering, the
working trailer rendering, the render free functions (5390–6157: chips,
detail bodies, subagent titles), the frame-stats helpers, and
`impl Render for Transcript` into `render.rs` as `impl Transcript` blocks.
`toggle_skill_fold` stays in the facade with the other fold/entity state.
Keep `Transcript`'s state ownership in `mod.rs`; when the child needs fields,
private fields of a facade-defined struct are already visible to child
modules — prefer that narrowest form, and only raise `pub(super)` when the
compiler requires it for free items. Keep all event emission in the facade so
`TranscriptEvent` remains the sole shell-facing event type.

Add explicit `pub use` lines in `mod.rs` for every item currently consumed via
`crate::transcript::...` (including `Row`, `RowKind`, `ToolItem`, public
constants, and pure helpers). Remove obsolete imports and the old monolithic
file only after the module tree builds.

**Verify**: `cargo test --workspace` → exit 0; `cargo clippy --workspace` →
exit 0 with baseline warnings only; run the per-package format check → exit 0;
`git diff --stat` shows the original implementation is distributed across the
listed children and no out-of-scope files changed.

## Test plan

- Relocate, without rewriting, all existing `#[cfg(test)]` modules that test
  `StickSpring`, row construction, parsing, viewport anchors, tool summaries,
  and formatting into the child owning each symbol.
- Keep tests that require private `Transcript` state in `mod.rs` or
  `render.rs` and use `pub(super)` only where the compiler requires it.
- Add no new behavior tests in this structural change; the existing 568 UI
  tests and workspace suite are the regression oracle. A changed test count is
  acceptable only if a test module was physically relocated and the executor
  reports the reason. The drift's new tests relocate with their symbols:
  `skill_file_display_collapses_home_and_strips_file_scheme` and
  `invocation_chip_opens_the_agent_entry_before_thinking` move with the row
  model into `model.rs`; shared test fixtures (`parse`, `assistant`,
  `text_part`, …) are duplicated verbatim into each child that needs them,
  matching the per-child `mod tests` convention in `composer/` and `pickers/`.

## Done criteria

- [ ] `crates/ui/src/transcript/mod.rs` is the only transcript module entry;
  no duplicate `transcript.rs` module remains.
- [ ] `model.rs`, `markdown.rs`, `viewport.rs`, `tool.rs`, and `render.rs`
  compile and contain only the responsibilities listed in this plan.
- [ ] Existing `crate::transcript::...` call sites compile without semantic
  changes or public API renames.
- [ ] `cargo test --workspace`, `cargo clippy --workspace`, and the documented
  per-package format check all exit 0.
- [ ] UI test count is unchanged except for explicitly relocated tests.
- [ ] `git status --short` lists only the in-scope files; `plans/README.md`
  marks plan 005 DONE or BLOCKED with a reason.

## STOP conditions

- Any current-state symbol or public call site differs from the excerpts or
  manifest after the drift check.
- Moving a symbol requires changing row IDs, fingerprints, rendered text,
  event payloads, scroll constants, or async scheduling behavior.
- A child module needs broad (`pub`) exposure of `Transcript` internals or a
  new cross-module state abstraction; stop and report the exact compiler error.
- Any verification command fails twice, or tests fail in a way not explained by
  test relocation.
- Compilation requires modifying an out-of-scope file other than unavoidable
  import visibility in `crates/ui/src/lib.rs`; stop before editing it.

## Maintenance notes

- Future transcript features should be placed in the child matching their
  primary responsibility; keep `mod.rs` focused on entity state, sync, and the
  shell-facing event/API surface.
- Reviewers should inspect that moved bodies are verbatim, that `pub use`
  preserves existing paths, and that child modules do not acquire cyclic
  ownership of `Transcript`.
- Attachment cache policy and syntax highlighting remain entity concerns unless
  a later change introduces a stable independent service; they are explicitly
  not redesigned here.
