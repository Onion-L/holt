# Plan 004: Split `pickers.rs` into cohesive picker modules

> **Executor instructions**: Follow this plan step by step. This is a structural
> refactor: preserve behavior and public paths unless a step explicitly says
> otherwise. Run every verification gate. If a STOP condition occurs, stop and
> report instead of redesigning the picker architecture.
>
> **Drift check (run first)**: `git diff --stat 481a273..HEAD -- crates/ui/src/pickers.rs crates/ui/src/pickers/`
> If the diff is non-empty, compare the current symbols with this plan before
> moving code; a mismatch is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `481a273`, 2026-09-03

## Why this matters

`crates/ui/src/pickers.rs` is a 4,036-line GPUI entity that owns four picker
surfaces, shared popup/focus/search state, catalog/ref RPC loading, draft
resolution, persistence, keyboard navigation, and rendering. The file is a
high-churn UI hotspot; unrelated changes must currently touch the same large
`Pickers` implementation and its 30-field state. Splitting by responsibility
will make future changes local and make pure behavior testable without creating
multiple synchronized GPUI entities.

## Current state

- `crates/ui/src/pickers.rs` — public facade, `Pickers` entity, all picker logic
  and rendering. `Pickers` is declared at lines 460–527.
- `crates/ui/src/composer.rs` and `crates/ui/src/composer/send.rs` — consume
  `Pickers`, `DraftConfig`, `CheckoutPlan`, and `ResolvedRunConfig`; preserve
  these paths and public names.
- `crates/ui/src/shell/spaces.rs` — consumes path-browser helpers from
  `crate::pickers`; preserve their public paths.
- `crates/ui/src/popover.rs` — shared `Loadable`, menu, focus, and overlay
  primitives; do not duplicate them.

Natural seams are: pure/domain helpers and types (lines 94–403 and 3460–3675);
catalog/ref loading (`ensure_providers`, `prefetch_models`, `ensure_models`,
`ensure_refs`, lines 990–1212); provider/model/traits behavior and views
(selection around 1358–1618, provider view at 2852, traits at 3317);
branch/checkout behavior and views (1216–1356, 2575–2840); space behavior/view
(1724–1875); and shared GPUI frame/chip/search/retry/scrollbar/overlay helpers
(1986–2574 and 3675–3710).

Keep one `Pickers` entity and one shared `Popup<PickerKind>`, search input,
focus handle, active row, and draft state. Use the existing module-directory
pattern in `crates/ui/src/composer/` and `crates/ui/src/shell/`. Child modules
may add `impl Pickers` blocks with the smallest `pub(super)` visibility needed.

Two suspicious behaviors are explicitly out of scope: `provider_locked()` is
currently a constant `false` (line 690), and the model retry condition
`attempt >= 1` (line 1092) prevents retries. Do not alter them while moving.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Drift check | `git diff --stat 481a273..HEAD -- crates/ui/src/pickers.rs crates/ui/src/pickers/` | empty, or STOP |
| Format check | `cargo fmt -p holt-ui -- --check` | exit 0, no output |
| Lint | `cargo clippy --workspace` | exit 0; only existing dependency warnings |
| UI tests | `cargo test -p holt-ui` | exit 0; baseline is 525 passed |
| Workspace tests | `cargo test --workspace` | exit 0 |

Do not run `cargo fmt --all`; the vendored `vendor/gpui` workspace makes that
command fail. Use `cargo fmt -p holt-ui` when formatting is needed.

## Scope

**In scope**

- `crates/ui/src/pickers.rs`
- New sibling files under `crates/ui/src/pickers/`: `logic.rs`, `catalog.rs`,
  `provider_model.rs`, `checkout.rs`, `space.rs`, and `common.rs` as needed.
- `plans/README.md` status row only.

**Out of scope**

- Composer, shell, engine, RPC, or `popover.rs` API changes.
- New GPUI entities, event buses, behavior/copy/visual changes, or
  `vendor/gpui` edits.

## Steps

### Step 1: Establish characterization coverage

Inventory every public symbol and external caller with `rg`. Add tests for
checkout-plan cases, model-row scoping/ranking, model normalization, provider
flattening, and existing path helpers. Assert current behavior; do not fix the
two deferred suspicious behaviors.

**Verify**: `cargo test -p holt-ui` → all tests pass; record the new count.

### Step 2: Extract pure logic and domain types

Create `pickers/logic.rs` and move `DraftConfig`, checkout types,
`ResolvedRunConfig`, default/traits helpers, path helpers, model row ranking and
normalization, provider icons, and `offered_providers`. Keep public names
available through `crate::pickers` with explicit `pub use`; do not depend on
GPUI types in this module.

**Verify**: `cargo fmt -p holt-ui -- --check` and `cargo test -p holt-ui` →
exit 0.

### Step 3: Extract stateful behavior into `impl Pickers` modules

Move methods without changing expressions or event semantics:

- `catalog.rs`: provider/model/ref load state and `ensure_*` methods.
- `provider_model.rs`: provider/model selection, row cache/navigation, and
  provider/model/traits rendering.
- `checkout.rs`: ref selection, branch creation, checkout selection, and
  branch/checkout popovers.
- `space.rs`: space filtering, selection, and space popover.
- `common.rs`: frame/chip/search/retry/scrollbar/overlay helpers.

Keep `Pickers::new`, `render`, and public facade methods in `pickers.rs` unless
compilation forces a minimal relocation. Keep the single shared search
subscription and state observers in `new`.

**Verify after each module**: `cargo check -p holt-ui` → exit 0. After all
moves: `cargo fmt -p holt-ui -- --check` → exit 0.

### Step 4: Preserve facade and audit the diff

Confirm existing imports compile unchanged, especially path helpers consumed by
`shell/spaces.rs`, `CheckoutPlan`, `DraftConfig`, `ResolvedRunConfig`, and
`Pickers` methods consumed by composer/send. Remove duplicate definitions and
ensure only module placement/visibility/wiring changed.

**Verify**: `cargo clippy --workspace` and `cargo test --workspace` → exit 0;
`git diff --stat` lists only Scope files.

## Test plan

Keep the existing `#[cfg(test)]` coverage as the baseline, moving tests with
pure functions or re-exporting them through the facade. Add cases for all three
`CheckoutPlan` variants, model search/scope/ranking, normalization alias and
1M handling plus idempotence, configured provider variants, and existing path,
branch-name, reasoning, and resolved-config behavior. The full UI test suite
is the regression gate for GPUI event wiring and rendering compilation.

## Done criteria

- [ ] `pickers.rs` is a facade/orchestrator; responsibilities live in sibling modules.
- [ ] One `Pickers` entity still owns popup, search, focus, draft, and async state.
- [ ] Existing public import paths and composer/shell callers compile unchanged.
- [ ] `cargo fmt -p holt-ui -- --check`, `cargo clippy --workspace`,
  `cargo test -p holt-ui`, and `cargo test --workspace` all pass.
- [ ] `git status --short` contains only in-scope files and the plan status row is updated.

## STOP conditions

- The drift check invalidates the symbol/line map.
- A move requires changing a public API or touching an out-of-scope crate.
- GPUI privacy/lifetime constraints require a second entity, duplicated search
  state, or an event bus.
- Any test or clippy failure remains after two reasonable correction attempts.
- A move changes runtime behavior, strings, ordering, loading states, or
  keyboard semantics rather than only file/module placement.

## Maintenance notes

Keep pure logic independent of GPUI so catalog and checkout behavior remains
cheap to test. New picker behavior should live in its own sibling module; add
shared state to `Pickers` only when interaction crosses picker boundaries. Fix
`provider_locked` and retry semantics in separate plans so this refactor stays
reviewable as a move-oriented change.
