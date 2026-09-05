# GPUI Debugging

This is a diagnosis playbook for the vendored GPUI snapshot used by Holt. Start
with a small reproduction and inspect the local implementation; do not infer
behavior from a newer GPUI release.

## Fast triage

| Symptom | First boundary to inspect | Usual correction |
| --- | --- | --- |
| State changed but pixels are stale | entity dependency and invalidation | mutate through `update`, call `cx.notify()` once, and ensure the view actually read that entity |
| `already being updated` / double-lease panic | nested `read`/`update` or deferred callback | use the direct `&mut` value inside the current lease; do not reacquire the same entity |
| Shortcut does nothing | focus path, `key_context`, action binding | keep a live `FocusHandle`, render `.track_focus`, bind the action in the active context, and verify propagation |
| Click/hover misses | stable `ElementId`, hitbox, paint order | give stateful elements a stable sibling-unique ID and register interaction in the correct phase |
| Inner scroll moves the outer list | event dispatch containment | put `.occlude()` on the nested scroll area; a bubble handler is too late |
| Async result updates a closed view | task ownership and weak handles | store cancellable tasks, capture `WeakEntity`, and handle fallible `update`/`update_in` |
| UI freezes during load/search | executor choice | move CPU or blocking work to `background_spawn`; return to the foreground only to update state |
| List rows show wrong hover/state | element identity and list cache | use domain keys instead of indexes and call `ListState::splice`/`reset` when heights or membership change |
| Geometry is wrong or overlay is clipped | render phase and content mask | compute layout in `request_layout`, bounds/hitboxes in `prepaint`, and use `with_content_mask` for intentional overflow |
| Frame time grows after a small change | invalidation breadth or repeated side effects | measure dirty views/list work, split high-frequency entities, and keep task/subscription creation out of `render` |

## Trace in this order

1. **State:** identify the owning `Entity<T>`, every `read`/`update`, and the
   exact mutation that should invalidate the view. A `notify` is not a redraw
   by itself; GPUI redraws views that observed the changed entity.
2. **Frame:** for custom elements, separate `request_layout -> prepaint ->
   paint`. Keep business state in entities and pass only frame-local caches
   through `RequestLayoutState`/`PrepaintState`.
3. **Input:** confirm the element is in the current dispatch tree, has a
   usable hitbox, and that focus/action capture or bubble order matches the
   intended owner. Actions normally stop during bubble unless the handler
   explicitly propagates.
4. **Lifetime:** inspect where each `Task` and `Subscription` is stored or
   detached. Dropping a `Task` cancels it; detaching removes the owner's
   cancellation boundary. Async entity updates are fallible because the app or
   view may have gone away.
5. **Performance:** distinguish excessive invalidation, layout/text shaping,
   paint work, and GPU present before changing abstractions. Prefer a smaller
   dependency surface or list renderer over a new custom element.

## Re-entrancy rules

`Entity<T>` leases are exclusive. Inside `entity.update(cx, |value, cx| { ... })`
use `value` directly. Do not call `entity.read(cx)` or `entity.update(cx, ...)`
again, even indirectly through a callback that still holds the lease.

The same rule applies to `defer_in`: the callback receives the deferred
entity's mutable value, so update it directly. Updating that same entity by
handle from inside the callback re-enters its lease and panics. Updating a
different entity is valid when its lifecycle is still known.

## Async checks

- Create tasks in constructors or explicit event methods, never as a render
  side effect.
- Save replaceable work in `Option<Task<_>>`; assigning a new task cancels the
  old one.
- Use `cx.spawn` for foreground coordination and UI updates; use
  `background_spawn` for `Send` CPU/I/O work.
- Capture a `WeakEntity` in long-lived callbacks and treat `update` failure as
  normal shutdown, not a reason to `unwrap`.
- In tests, drive async work with `run_until_parked()` and avoid accidental
  parking on external runtimes unless the test explicitly allows it.

## Input and nested scroll checks

For a missing key or click, verify in order: the action type is registered, the
key binding's context matches `.key_context(...)`, the focused element is
mounted with `.track_focus(...)`, and no child handler stops propagation first.

For a scrollable child inside `list(...)`, use:

```rust
div()
    .id("reading-viewport")
    .overflow_y_scroll()
    .occlude()
    .child(content)
```

The `.occlude()` is an event-dispatch boundary. Keep a regression test that
asserts the child offset changes while the parent list offset does not.

## Regression test choice

- Pure reducers, derived values, and ordering: ordinary Rust unit test.
- Entity notifications, subscriptions, and task cancellation: `#[gpui::test]`
  with `TestAppContext`.
- Render, focus, key dispatch, hitboxes, or scroll: `VisualTestContext` and a
  simulated input event.
- Custom `Element`: cover bounds, content masks, hit testing, and paint-state
  reuse; keep the test independent of unrelated application state.

When the bug is fixed, leave the smallest test that would fail again if the
same boundary regresses. Record the relevant local source path in the test or
nearby comment only when the invariant is otherwise non-obvious.

## Local evidence commands

```text
rg "cx\\.notify|cx\\.spawn|background_spawn|on_action|track_focus|occlude" crates/ui vendor/gpui
rg "fn request_layout|fn prepaint|fn paint" vendor/gpui/crates/gpui/src
cargo test -p holt-ui -- <focused-test-name>
```

Read `docs/research/gpui.md` and `docs/adr/0013-nested-scroll-containers-isolate-wheel-events.md`
when the issue involves Holt's frame model or nested scrolling.
