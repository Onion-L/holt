---
name: gpui
description: "Explain and apply the vendored GPUI framework: entities and contexts, declarative views, custom elements, layout, input/focus, async tasks, lists, and tests. Use when building or changing a GPUI UI, investigating rendering/layout/event/async bugs, or resolving an API mismatch in a pre-1.0 GPUI snapshot."
---

# GPUI

Use this skill as a source-first implementation and diagnosis guide. GPUI is
pre-1.0 and Holt uses a patched snapshot, so verify APIs against the checkout.

## Source order

1. `vendor/gpui` source and examples (Holt's API truth).
2. Existing `crates/ui` call sites and `docs/research/gpui.md`.
3. Upstream docs only for background; confirm signatures locally.

## Quick start

Put durable state in an `Entity<T>`, render with `Render`, and notify after a
meaningful mutation:

```rust
fn increment(&mut self, cx: &mut Context<Self>) {
    self.value += 1;
    cx.notify();
}

impl Render for Counter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().id("counter").child(self.value.to_string())
    }
}
```

Prefer existing elements and `RenderOnce`; use a custom `Element` only when
manual layout, hit testing, or painting is required.

## Feature workflow

1. Classify it as state/entity, render/layout, action/focus, async, list,
   platform, or custom element; load the matching reference below.
2. Find the closest local example with `rg`. Keep durable state in entities,
   stable IDs on stateful elements, and task/subscription handles in their owner.
3. Make the smallest change, then run a focused check/test and format. Done
   means behavior and lifecycle have a reproducible test or existing invariant.

## Bug workflow

1. Reproduce the smallest interaction/test and classify it as stale UI, bad
   geometry, missed input, panic, race, or jank.
2. Follow [debugging.md](references/debugging.md), then trace the relevant
   local boundary: invalidation, render phase, dispatch tree, task lifetime, or
   list state.
3. Add the lowest-layer regression test: `VisualTestContext` for window/input;
   ordinary Rust tests for pure state.

## Navigation

Load the relevant reference file based on the task:

| Topic | File | When to load |
|-------|------|--------------|
| Actions & keybindings | [action.md](references/action.md) | `actions!`, `bind_keys`, `on_action`, `key_context` |
| Async & background tasks | [async.md](references/async.md) | `cx.spawn`, `background_spawn`, `Task`, async I/O |
| Context management | [context.md](references/context.md) | `App`, `Window`, `Context<T>`, `AsyncApp` |
| Custom elements (low-level) | [element.md](references/element.md) | `Element` trait, `request_layout`, `prepaint`, `paint` |
| Entity state | [entity.md](references/entity.md) | `Entity<T>`, `WeakEntity`, state management |
| Events & subscriptions | [event.md](references/event.md) | `cx.emit`, `cx.subscribe`, `cx.observe` |
| Focus & keyboard nav | [focus-handle.md](references/focus-handle.md) | `FocusHandle`, `track_focus`, Tab navigation |
| Global state | [global.md](references/global.md) | `Global` trait, `cx.set_global`, app-wide config |
| Layout & styling | [layout-style.md](references/layout-style.md) | `div()`, flexbox, overflow, positioning, SVG icon styling |
| ElementId | [element-id.md](references/element-id.md) | `ElementId`, `.id()`, uniqueness rules, stateful elements |
| Testing | [test.md](references/test.md) | `#[gpui::test]`, `TestAppContext`, `VisualTestContext` |
| Debugging | [debugging.md](references/debugging.md) | stale UI, layout/input bugs, panics, async races, jank, regression tests |

## Extended References

For deep-dive topics, additional reference files are available:

**Element trait:**
- [element-api.md](references/element-api.md) — complete API, hitbox system, event handling
- [element-patterns.md](references/element-patterns.md) — text, interactive, container, composite patterns
- [element-examples.md](references/element-examples.md) — full examples: text, interactive, complex elements
- [element-best-practices.md](references/element-best-practices.md) — performance, state, common pitfalls
- [element-advanced.md](references/element-advanced.md) — masonry/circular layouts, async updates, virtual lists

**Entity management:**
- [entity-api.md](references/entity-api.md) — complete Entity API, methods, lifecycle
- [entity-patterns.md](references/entity-patterns.md) — model-view, cross-entity communication, observer
- [entity-best-practices.md](references/entity-best-practices.md) — memory, performance, lifecycle
- [entity-advanced.md](references/entity-advanced.md) — collections, registry, debounce, state machines

**Testing:**
- [test-examples.md](references/test-examples.md) — testing examples and patterns
- [test-reference.md](references/test-reference.md) — complete testing API reference

In Holt, start with `cargo test -p holt-ui` or `cargo check -p holt-ui`, then
run `cargo fmt --all`; widen to workspace check/clippy for cross-crate changes.
