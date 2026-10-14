# GPUI Principles, Usage, and Best Practices

> Survey date: 2026-08-30. This document treats the vendored GPUI snapshot in the Holt repository as the API truth and is written for engineers already familiar with Rust and common frontend state management.

## 1. Conclusions up front

GPUI is a Rust-native, GPU-accelerated UI framework that mixes immediate and retained mode. It is not a DOM framework, and it does not put long-lived business state into the component tree:

- `App` is the runtime and the owner of state; `Entity<T>` is a strongly typed handle managed by `App` that requires a context to read or write.
- `Entity<T: Render>` is a view. Once a view is marked dirty, `Render::render` re-declares the element tree; unchanged views can reuse the previous frame's prepaint/paint data.
- An element is a single-frame object that works through `request_layout -> prepaint -> paint`; cross-frame state lives in an entity, or in the element state associated with a stable `ElementId`.
- Input first forms hitboxes, the focus path, and the dispatch tree; business operations should be modeled as `Action`s, then triggered from keys, menus, or the mouse via the same action/method.
- `cx.spawn` runs local futures on the foreground; `background_executor().spawn` runs `Send` futures in the background. Dropping a `Task` cancels it; only call `detach()` when the task genuinely needs to survive on its own.
- The crate is still pre-1.0, and Holt uses a patched, frozen snapshot, so do not guess the API from the latest examples online.

These conclusions are jointly supported by GPUI's own README, the ownership document, and the local implementation. [S1][S2][S3][S6][S7][S11] The snapshot status of Holt's fork is described in the repository architecture notes. [S0]

## 2. Version and source boundaries

The local `gpui` crate declares version `0.2.2`; its default features include `font-kit`, Wayland, X11, and the Windows manifest; tests, benchmarks, and the profiler are separate features. [S17] `ARCHITECTURE.md` further states that `third_party/gpui` is a frozen snapshot of the Zed fork, carries the glass/edge-fade patches Holt depends on, and is never resolved from git. [S0]

The official README explicitly warns that GPUI is still under rapid development and that breaking changes between versions are frequent. [S1] This document therefore prioritizes:

1. The sources and examples in `third_party/gpui`;
2. Holt's current call sites;
3. The official Zed repository and `gpui.rs`, used only for background and cross-checking.

When checked on 2026-08-30, Zed's official `main` (commit `399258feeaf90ad8a3a208c99221ee87b6452f38`) carried a GPUI README and contexts document matching the corresponding local texts. [U1][U2] That does not mean every source-level API is identical.

## 3. Runtime model and architecture

### 3.1 From the platform event loop to a frame

The startup chain is:

```text
gpui_platform::application()
  -> picks the Platform implementation for the current OS
  -> Application::run
  -> App::open_window creates the root view
  -> platform request-frame/input callbacks
  -> Window::draw
  -> root element: request_layout -> prepaint -> paint
  -> Scene handed to PlatformWindow::draw
```

`gpui_platform::current_platform` selects different implementations on macOS, Windows, Linux/FreeBSD, and wasm; `Platform` provides the event loop, windows, the text system, and executors, while `PlatformWindow` receives input and frame requests and ultimately submits the `Scene`. [S12] `Application::run` merely lends `App` to the startup callback; from there the platform run loop drives everything. [S5]

`Window::draw` first processes invalidated entities, clears this frame's access records, and then calls `draw_roots`. The root element first requests a Taffy layout, then prepaints, then paints; afterwards the text system finishes the frame and swaps `rendered_frame` / `next_frame`, and `PlatformWindow::draw` finally submits the scene. [S8]

The responsibilities of the three element phases must be kept distinct: [S6][S8]

| Phase | What it does | What it must not do |
| --- | --- | --- |
| `request_layout` | Declares the Taffy layout node and returns layout-phase state | Rely on final bounds; register interactions that are only valid after hit-testing |
| `prepaint` | Receives bounds, builds hitboxes and dispatch/focus metadata, prepares draw data | Escape the parent content mask casually; perform heavy business I/O |
| `paint` | Writes primitives into the scene and registers listeners that depend on the final draw order | Mutate layout; hide business state inside a single-frame element |

### 3.2 Why it is called hybrid immediate/retained

The "immediate" side: every time a view actually redraws, `Render::render` recreates the element tree from the current state; that tree, and the callbacks registered in it, are discarded before the next frame. [S1][S6]

The "retained" side shows up at three levels:

- `App` holds entity data for the long term; the strong reference count of `Entity<T>` decides when an entity is released. [S3][S5]
- An element with a stable `ElementId` can access the same element state across adjacent frames; built-in interactions such as hover, clicked, and image state rely on this mechanism. [S6]
- `View` records the entities it has read plus its prepaint/paint ranges. When bounds, content mask, and text style are unchanged and the view is not dirty, GPUI calls `reuse_prepaint` / `reuse_paint` and skips re-rendering and draw-list construction. [S7][S8]

`cx.notify()` is therefore not "redraw the whole window right now". It marks the entity as changed; the window records the dependency and marks the corresponding view and its ancestors dirty; multiple invalidations before the same frame can be coalesced. [S4][S8]

## 4. Entity, Context, and data flow

### 4.1 Core types

| Type | Lifetime / capabilities | Typical use |
| --- | --- | --- |
| `App` | Application-wide mutable context; owns entities, globals, windows, executors | Creating entities/windows, accessing platform services, registering global behaviors |
| `Context<T>` | `App` plus the current entity; dereferences to `App` | `notify`, `emit`, `observe`, `subscribe`, entity-scoped `spawn` |
| `Entity<T>` | Strongly typed strong handle; cannot dereference `T` by itself | Accessing state through `read` / `update` |
| `WeakEntity<T>` | Does not extend the entity's lifetime; `upgrade/update` can fail | Async callbacks, long-lived listeners; prevents strong-reference cycles |
| `AsyncApp` / `AsyncWindowContext` | Can be held across `await`; access may fail | Foreground async tasks returning to UI state |
| `Window` | Window state and drawing/input services; not a context | focus, layout, paint, clipboard, window-scoped API |
| `Global` | Application-wide state stored in `App`, keyed by type | theme, settings, shared services; not suitable for per-view transient state |

GPUI's official contexts document lays out these boundaries; the `Context<T>` implementation also explicitly holds a `&mut App` and the current `WeakEntity<T>`, and implements `Deref<Target = App>`. [S2][S4]

### 4.2 Read, update, notify, and events

Basic rules:

```rust
let model = cx.new(|_| Model::default());

let snapshot = model.read(cx).value;

model.update(cx, |model, cx| {
    model.value += 1;
    cx.notify();
});
```

`Entity<T>` is not a synonym wrapper around `Rc<RefCell<T>>`. The state actually lives in `App`'s entity map; `read` borrows immutably, while `update` leases the entity out temporarily and provides a `Context<T>`. Re-entrantly reading or writing the same entity triggers the double-lease panic, so do not `read/update` an entity again inside its own `update` closure. [S3][S18]

Do not mix the two communication semantics:

- `notify` / `observe` means "this entity's observable state changed"; it is mainly used to invalidate the views that depend on it. [S3][S4]
- `emit` / `subscribe` means "a typed domain event occurred"; the sender must implement `EventEmitter<Event>`. [S3][S4]

Dropping a `Subscription` cancels it; storing it in a field of the struct that owns the listening relationship gives the clearest lifetime. `detach()` keeps the listening alive until the related entity is released — appropriate for relationships that truly live as long as both entities, not something to call casually to silence a `must_use` warning. [S15]

### 4.3 Recommended state layering

Engineering advice based on the ownership and invalidation mechanics above:

- Put domain state, async results, and state shared across views in entities; compute purely derived values with pure functions wherever possible.
- Put theme, settings, and process-level services in a `Global`, but do not cram all UI state into one mutable global.
- Put focus, scroll, selection, and in-flight `Task`s that belong to a single view in that view/entity.
- Use `RenderOnce` for purely presentational components; do not create an entity for every label and badge.
- Prefer typed events for cross-layer notifications; if the receiver only needs a re-render, use `observe + notify`.

## 5. The view and element system

### 5.1 Order of preference

Choose abstractions from highest to lowest:

1. Built-in elements such as `div()`, `text`, `svg`, `img`, `list`;
2. `#[derive(IntoElement)] + RenderOnce` for reusable components with no long-lived state;
3. `Entity<T: Render>` for views with identity and state that need observation/async tasks;
4. Implement `Element` / use `canvas` only when you need custom layout, text input, hit-testing, or direct drawing.

This is the recommended path stated explicitly in the `element.rs` module docs: most business code should compose built-in elements; write a custom element only when you need manual control over layout/paint. [S6]

### 5.2 Stable IDs and cross-frame state

Interactive elements should be given a stable `.id(...)` unique among siblings. `Element::id` forms a hierarchical `GlobalElementId`, which GPUI uses to access cross-frame state; duplicate IDs under the same parent ID mis-associate state. [S6]

In practice:

- Use domain-stable keys for list item IDs, e.g. `("message", message.id.clone())`, not indices that shift on insertion.
- Avoid reusing the same ID when conditional branches swap in different controls, unless they really are the same logical control.
- When hover, active, scroll handles, image caches, and the like depend on element state, make sure the ID does not change arbitrarily between renders.

### 5.3 The boundaries of custom elements

A custom element's `RequestLayoutState` carries layout-phase data, and `PrepaintState` carries draw-phase data such as bounds/hitbox/shaped text. [S6] Keep both of these as "computed-this-frame caches"; real business state should still be held by an entity.

A custom element may escape the parent bounds, but it must explicitly use `Window::with_content_mask`; this is what enables popovers/overlays, and it also means the author is responsible for clipping, z-order, and hit behavior. [S6]

## 6. Events, actions, and focus

### 6.1 Two kinds of events

- Raw input: `on_mouse_down`, `on_click`, `on_key_down`, and the like — suitable for gestures and control-local behavior.
- `Action`: application semantics, e.g. `Save`, `MoveDown`, `ClosePanel`. It can be dispatched from a key binding, a menu, or code, and suits business commands.

GPUI's key dispatch is keyboard-first by design: define actions with `actions!` or `#[gpui::action]`, handle them on elements with `on_action`, and use `key_context` to constrain where key bindings apply. [S9][S10]

### 6.2 Focus path and propagation

A focusable view usually holds a `FocusHandle`, implements `Focusable`, and calls `.track_focus(&handle)` in the render tree. Actions registered around that focus path resolve as expected only when the handle appears inside the current dispatch tree. [S10][S13]

Propagation splits into capture and bubble: keyboard capture runs from the root toward the focus, bubble from the focus toward the root; mouse ordering is handled as the reverse of z-order. Ordinary business listeners usually use bubble. [S8] Actions in nested views bubble from the deepest level upward, so editor-level bindings take precedence over pane/workspace-level bindings. [S10]

Note the differing defaults: ordinary events propagate by default and can be stopped with `cx.stop_propagation()`; actions stop by default in the bubble phase and require an explicit `cx.propagate()` to continue to parent or global handlers. [S5]

### 6.3 Recommended pattern

```rust
actions!(counter, [Increment]);

impl Counter {
    fn increment(
        &mut self,
        _: &Increment,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.value += 1;
        cx.notify();
    }
}

impl Render for Counter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("counter")
            .track_focus(&self.focus)
            .key_context("Counter")
            .on_action(cx.listener(Self::increment))
            .child(self.value.to_string())
    }
}
```

A mouse button can call the same `increment`, or dispatch the same `Increment`; do not duplicate the state-mutation logic. The example interface comes from the current snapshot's `examples/testing.rs`. [S13]

## 7. Async tasks

### 7.1 Foreground, background, and Tokio

| API | Where it runs | Constraint | Use case |
| --- | --- | --- | --- |
| `cx.spawn(...)` | GPUI foreground executor / main thread | The future may be `!Send`; returns to the UI via `AsyncApp` | Coordinating UI state, awaiting channels/timers |
| `cx.background_executor().spawn(future)` or `cx.background_spawn(...)` | Background scheduler | Future/output must be `Send` | CPU work, thread-safe I/O |
| `gpui_tokio::Tokio::spawn(cx, future)` | Tokio runtime | Follows Tokio's requirements | Libraries that depend on the Tokio reactor/driver |

GPUI's executors are integrated with the platform event loop; the foreground executor is explicitly the main thread, and the background executor queues `Send` futures onto background threads. [S1][S11] Holt currently calls `gpui_tokio::init(cx)` first and puts only the engine bootstrap/shutdown — the parts that need a Tokio runtime — onto Tokio; the runtime-agnostic RPC channel pump stays in `cx.spawn`. [S16]

### 7.2 The Task lifecycle is the cancellation policy

`Task<T>` implements `Future`; dropping it cancels it immediately, `await`ing yields the result, and only `detach()` lets the task keep running independent of the handle. [S11] Recommendations:

- Loads, searches, and completions that a new request can supersede: store them as `Option<Task<()>>`; assigning a new value cancels the old task automatically.
- Loops that must live as long as the view: also store the task as a field; it cancels automatically when the view is released.
- Detach only genuinely fire-and-forget tasks whose errors are already handled; prefer `detach_and_log_err` for `Task<Result<...>>`.
- Use `WeakEntity`/`this.update(...)` in async closures and accept that the view may already be released; do not force an `unwrap`.

The pattern under the current API: [S4][S11][S13]

```rust
fn reload(&mut self, cx: &mut Context<Self>) {
    self.reload_task = Some(cx.spawn(async move |this, cx| {
        let result = fetch().await;
        this.update(cx, |this, cx| {
            this.result = Some(result);
            cx.notify();
        })
        .ok();
    }));
}
```

Do not run long synchronous computation inside a foreground task; it still occupies the UI thread. Put heavy computation on the background executor and update the entity through the async context after awaiting the result.

## 8. Window and the platform layer

`Window` manages the root view, viewport, focus, dispatch tree, hitboxes, scene, text layout, and the platform window. It is not a context, so entity operations still require passing `&mut App`/`Context<T>` alongside it. [S2][S8]

`Platform`/`PlatformWindow` is the OS boundary, covering window creation, input, displays, clipboard, menus, file dialogs, URLs, the text system, the GPU atlas, window decorations, and lifecycle. [S12] Prefer GPUI's cross-platform APIs over calling Cocoa/Win32/X11 directly from business views. Where differences are real:

- Keep `cfg(target_os)` confined to initialization or a very thin platform adapter;
- Verify titlebar/decorations behavior per platform when configuring `WindowOptions`;
- Use `on_app_quit` to run bounded-duration async cleanup; do not spawn foreground tasks during the shutdown phase — the current implementation rejects them. [S4][S5]

Holt's `run_app` already follows this structure: it initializes the platform/application and globals, creates a single `AppState`, opens the root window with that entity, and reuses the same state on macOS reopen. [S16]

## 9. A full working skeleton (current vendored API)

The skeleton below uses APIs straight from the current `examples/testing.rs` and Holt's startup path: [S13][S16]

```rust,no_run
use gpui::{
    App, AppContext as _, Context, FocusHandle, Focusable, KeyBinding, Render, Task,
    Window, actions, div, prelude::*,
};

actions!(counter, [Increment]);

struct Counter {
    value: i32,
    focus: FocusHandle,
    reload_task: Option<Task<()>>,
}

impl Counter {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            value: 0,
            focus: cx.focus_handle(),
            reload_task: None,
        }
    }

    fn increment(&mut self, _: &Increment, _: &mut Window, cx: &mut Context<Self>) {
        self.value += 1;
        cx.notify();
    }
}

impl Focusable for Counter {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Counter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("counter")
            .track_focus(&self.focus)
            .key_context("Counter")
            .on_action(cx.listener(Self::increment))
            .on_click(cx.listener(|this, _, window, cx| {
                this.increment(&Increment, window, cx);
            }))
            .child(self.value.to_string())
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        cx.bind_keys([KeyBinding::new("up", Increment, Some("Counter"))]);
        cx.open_window(Default::default(), |window, cx| {
            let counter = cx.new(Counter::new);
            counter.focus_handle(cx).focus(window, cx);
            counter
        })
        .unwrap();
    });
}
```

## 10. Lists, caching, and performance

### 10.1 Large lists

Use `list(ListState, renderer)` for large lists with varying row heights. It caches row heights; if an off-screen row changes its height, the caller must inform it via `ListState::splice` or `reset`. Prefer `uniform_list` for rows of equal height — the interface and the math are simpler. [S14]

Recommendations:

- Align the unit of virtualization with the business unit that is most often updated/collapsed; row-level diffs suit `list`, fixed-height tables suit `uniform_list`.
- Renderers should capture only the entities/lightweight data they need; avoid cloning an entire large model for every visible row.
- Use stable item IDs; keep list state in sync on insert/delete — mutating the backing vector alone is not enough.
- Move expensive parse/shaping of not-yet-visible data into caches or background tasks; do not rely on "the list will not render it anyway" to paper over upstream work.

### 10.2 Avoiding needless invalidation

- Call `notify` only after state that affects observers/rendering has actually changed; notify once after a batch of mutations.
- Put small, frequently changing state into smaller child entities, so a single notify on the root entity does not dirty too many views.
- Keep `RenderOnce` components pure; cache expensive pure computations in an entity first, then render the cached result.
- Do not call the global `refresh_windows` in place of proper entity notifications; it forfeits fine-grained reuse.
- Animations that need per-frame refresh are the exception, but you should still constrain the animated subtree and its lifetime.

GPUI tracks which entities a view reads and reuses unchanged subtrees, but that is not a reason to ignore data boundaries; the smaller the dependency surface, the smaller the set of dirty views. [S7][S8]

### 10.3 Measuring

The local crate exposes the `bench`, `profiler`, and `input-latency-histogram` features; the window internally records dirty-to-draw times and the number of coalesced invalidations. [S8][S17] Performance work should proceed in order:

1. Use the profiler/benchmarks to determine whether the cost is state invalidation, layout, text shaping, paint, or GPU present;
2. Shrink entity dependencies and the list's rendered range;
3. Cache pure computations / text-layout inputs;
4. Only then write a custom element or drop to a low-level canvas.

## 11. Testing strategy

`#[gpui::test]` provides a `TestAppContext`; when a test involves window/render/focus/action, construct a `VisualTestContext` from the window. Synchronous side effects run after `update`; the test executor is single-threaded, so async or detached tasks need an explicit `run_until_parked()`; futures that GPUI cannot drive — external threads/I/O and the like — will surface parking/deadlock failures by default; opt into `allow_parking()` only when truly needed. [S13]

Recommended test layers:

1. Pure reducers/sorting/derived functions: plain Rust unit tests — fastest and independent of GPUI.
2. Entity data flow: `#[gpui::test]`, verifying `update/notify/emit/subscribe` and task cancellation.
3. View behavior: `VisualTestContext`, dispatching actions through focus handles and verifying render-dependent state.
4. Critical custom elements: test layout, hitboxes, input method, and selection boundaries.
5. Concurrent/async state machines: use multiple `TestAppContext`s, `iterations`, and controlled dispatchers to explore scheduling orders.

Do not turn every verification into a screenshot test. GPUI's official examples focus on testing state, action dispatch, and the deterministic executor; visual snapshots suit a small set of stable components. [S13]

## 12. Anti-pattern checklist

| Anti-pattern | Consequence | Alternative |
| --- | --- | --- |
| Copying APIs from the latest blog post into Holt | Pre-1.0 APIs mismatch the fork's patches | Check `third_party/gpui` and local examples first |
| Starting tasks, subscriptions, or entity mutations inside `render` | Side effects repeat on every redraw and can form loops | Create and store the handles in a constructor/explicit event method |
| Forgetting `notify` after a mutation | The view keeps stale output | Notify once at the end of one logical mutation |
| Notifying continuously even when nothing changed | Loses view cache and paint reuse | Compare old/new values or return a changed flag from the reducer |
| Calling `.detach()` on every subscription | No explicit way to stop; murky lifetimes | Store `Subscription` fields |
| Calling `.detach()` on every task | Work continues after the view is released; results/errors go unread | Store/await tasks; detach only genuinely independent ones |
| Holding the view strongly or unwrapping update in async | Extends lifetimes, or panics after the window/entity closes | Use `WeakEntity`; handle the fallible update |
| CPU-heavy work in a foreground future | Blocks the event loop and drawing | Compute on the background executor; commit results on the foreground |
| Using array indices as IDs for dynamic rows | hover/focus/element state cross-wire after insertion | Use domain-stable keys |
| Implementing `Element` directly for ordinary components | layout/paint/hit-testing complexity grows | `RenderOnce` + built-in elements |
| Writing business logic directly in raw key handlers | Menu/shortcut/button behavior diverges | Define an `Action` or a shared entity method |
| Global refresh instead of dependency notifications | Whole-window invalidation; performance issues hard to localize | `cx.notify()` + small entity boundaries |

These alternatives follow from the element lifecycle, subscription/task drop semantics, key dispatch, and view-reuse mechanics respectively. [S6][S7][S9][S11][S15]

## 13. Recommendations for Holt

Holt's current direction broadly fits the GPUI model: a single root `AppState` connects to the engine, pure derivations live in `holt_proto::view`, GPUI tasks fold RPC frames into entities, and window-level views redraw by observing state. [S16]

Prioritized recommendations going forward:

1. **Stay vendored-API-first.** Before adding a new GPUI usage, search `third_party/gpui/crates/gpui/examples` and local call sites first; treat upgrades as standalone work that records the fork patches rather than partially mixing in upstream APIs. [S0][S1]
2. **Split high-frequency dependencies.** `AppState` is large; if the profiler shows root-state notifications dirtying views broadly, split high-frequency transcript, terminal, changes, and the like into child entities first, keeping only handles and navigation selection in the root state. Let dirty-view data decide whether to split. [S7][S8][S16]
3. **Keep pure reducers testable without GPUI.** `state.rs` already does this explicitly; keep letting GPUI glue handle only subscription/task/notify. [S16]
4. **Systematize task ownership.** Store replaceable tasks — search/load/parse and the like — as fields; detach only app-level work such as the URL pump and scheme registration. Holt already has many `Option<Task<_>>`; keep unifying on that convention. [S11][S16]
5. **Draw clear runtime boundaries.** Engine/transport work that depends on the Tokio driver goes through `gpui_tokio::Tokio::spawn`; ordinary channel coordination and UI updates go through `cx.spawn`; CPU-bound parsing/image work goes to the background executor. [S11][S16]
6. **Preserve virtualization constraints.** If variable-height rows in `changes.rs` change while off-screen, call `ListState::splice/reset` in sync; prefer `uniform_list` for new fixed-height tables. [S14]
7. **Reuse semantic actions.** `app_menus.rs`, `shell.rs`, and `composer.rs` already use actions/key contexts; make new buttons/menus call the same command path instead of shortcut-only logic. [S9][S16]
8. **Pair custom elements with low-level tests.** `composer.rs`, markdown selection, and frost/edge-fade depend on the request-layout/prepaint/paint boundaries; when changing them, cover at least bounds, focus/action, scroll/selection, and masks — not just pure state. [S6][S8][S13]
9. **Store subscriptions in their owner by default.** Holt already widely uses fields like `_observe: Subscription`; keep that pattern and detach only app-level listeners that are certain to share the entity's lifetime. [S15][S16]
10. **Fold the performance toggles into dedicated diagnostics.** When jank appears, enable the vendored `profiler`/input-latency capabilities, record invalidation counts and frame timing, then decide whether to split entities, change the list, or cache text/images. [S8][S17]

## 14. Primary-source index

### In this repository

- [S0] [`ARCHITECTURE.md:55-62`](../../ARCHITECTURE.md#L55-L62): the vendored fork, the frozen snapshot, and the role of the docs.
- [S1] [`third_party/gpui/crates/gpui/README.md:1-98`](../../third_party/gpui/crates/gpui/README.md#L1-L98): positioning, startup path, the entity/view/element three layers, executors, and testing.
- [S2] [`third_party/gpui/crates/gpui/docs/contexts.md:1-33`](../../third_party/gpui/crates/gpui/docs/contexts.md#L1-L33): responsibilities of each context, `Window`, and `Entity`.
- [S3] [`third_party/gpui/crates/gpui/src/_ownership_and_data_flow.rs:1-138`](../../third_party/gpui/crates/gpui/src/_ownership_and_data_flow.rs#L1-L138): ownership, read/update, observe/notify, subscribe/emit.
- [S4] [`third_party/gpui/crates/gpui/src/app/context.rs:20-269`](../../third_party/gpui/crates/gpui/src/app/context.rs#L20-L269), [`Context::emit:765-779`](../../third_party/gpui/crates/gpui/src/app/context.rs#L765-L779): entity context, listener, spawn, and events.
- [S5] [`third_party/gpui/crates/gpui/src/app.rs:225-309`](../../third_party/gpui/crates/gpui/src/app.rs#L225-L309), [`App::spawn:1847-1895`](../../third_party/gpui/crates/gpui/src/app.rs#L1847-L1895), [`App::on_action:2145-2185`](../../third_party/gpui/crates/gpui/src/app.rs#L2145-L2185): run, executors, propagation semantics.
- [S6] [`third_party/gpui/crates/gpui/src/element.rs:1-184`](../../third_party/gpui/crates/gpui/src/element.rs#L1-L184): element lifecycle, custom elements, `Render`/`RenderOnce`.
- [S7] [`third_party/gpui/crates/gpui/src/view.rs:285-481`](../../third_party/gpui/crates/gpui/src/view.rs#L285-L481): view cache, dirty checks, prepaint/paint reuse.
- [S8] [`third_party/gpui/crates/gpui/src/window.rs:88-205`](../../third_party/gpui/crates/gpui/src/window.rs#L88-L205), [`Window::draw:2719-2868`](../../third_party/gpui/crates/gpui/src/window.rs#L2719-L2868), [`draw_roots:2893-2970`](../../third_party/gpui/crates/gpui/src/window.rs#L2893-L2970): propagation, invalidation, the frame pipeline, and scene submission.
- [S9] [`third_party/gpui/crates/gpui/docs/key_dispatch.md:1-88`](../../third_party/gpui/crates/gpui/docs/key_dispatch.md#L1-L88): action, key context, binding.
- [S10] [`third_party/gpui/crates/gpui/src/key_dispatch.rs:1-50`](../../third_party/gpui/crates/gpui/src/key_dispatch.rs#L1-L50), [`DispatchTree:68-223`](../../third_party/gpui/crates/gpui/src/key_dispatch.rs#L68-L223), [`FocusHandle/Focusable`](../../third_party/gpui/crates/gpui/src/window.rs#L383-L565): nested actions, the dispatch tree, and focus.
- [S11] [`third_party/gpui/crates/gpui/src/executor.rs:1-122`](../../third_party/gpui/crates/gpui/src/executor.rs#L1-L122), [`third_party/gpui/crates/scheduler/src/executor.rs:178-339`](../../third_party/gpui/crates/scheduler/src/executor.rs#L178-L339): foreground/background executors, `Task` cancellation and detach.
- [S12] [`third_party/gpui/crates/gpui_platform/src/gpui_platform.rs:1-60`](../../third_party/gpui/crates/gpui_platform/src/gpui_platform.rs#L1-L60), [`Platform`](../../third_party/gpui/crates/gpui/src/platform.rs#L125-L320), [`PlatformWindow`](../../third_party/gpui/crates/gpui/src/platform.rs#L794-L958): platform selection and the abstraction surface.
- [S13] [`third_party/gpui/crates/gpui/examples/testing.rs:1-346`](../../third_party/gpui/crates/gpui/examples/testing.rs#L1-L346): a complete view/action/async/test example under the current API.
- [S14] [`third_party/gpui/crates/gpui/src/elements/list.rs:1-37`](../../third_party/gpui/crates/gpui/src/elements/list.rs#L1-L37), [`uniform_list.rs:22-126`](../../third_party/gpui/crates/gpui/src/elements/uniform_list.rs#L22-L126): constraints of variable-height/fixed-height lists.
- [S15] [`third_party/gpui/crates/gpui/src/subscription.rs:147-194`](../../third_party/gpui/crates/gpui/src/subscription.rs#L147-L194): subscription drop/detach semantics.
- [S16] [`crates/ui/src/lib.rs:75-185`](../../crates/ui/src/lib.rs#L75-L185), [`crates/ui/src/state.rs:1-18`](../../crates/ui/src/state.rs#L1-L18): Holt startup, runtime bridging, and state layering.
- [S17] [`third_party/gpui/crates/gpui/Cargo.toml:1-42`](../../third_party/gpui/crates/gpui/Cargo.toml#L1-L42): local version and features.
- [S18] [`third_party/gpui/crates/gpui/src/app/entity_map.rs:134-207`](../../third_party/gpui/crates/gpui/src/app/entity_map.rs#L134-L207), [`Entity/WeakEntity:414-800`](../../third_party/gpui/crates/gpui/src/app/entity_map.rs#L414-L800): entity lease/read, strong/weak handles, and the double-lease check.

### Official external sources

- [U1] [Zed GPUI README (pinned to 399258f)](https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/gpui/README.md)
- [U2] [Zed GPUI contexts (pinned to 399258f)](https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/gpui/docs/contexts.md)
- [U3] [Official GPUI API documentation](https://gpui.rs/)
- [U4] [The GPUI crate in the official Zed repository](https://github.com/zed-industries/zed/tree/main/crates/gpui)

[S0]: ../../ARCHITECTURE.md#L55-L62
[S1]: ../../third_party/gpui/crates/gpui/README.md#L1-L98
[S2]: ../../third_party/gpui/crates/gpui/docs/contexts.md#L1-L33
[S3]: ../../third_party/gpui/crates/gpui/src/_ownership_and_data_flow.rs#L1-L138
[S4]: ../../third_party/gpui/crates/gpui/src/app/context.rs#L20-L269
[S5]: ../../third_party/gpui/crates/gpui/src/app.rs#L225-L309
[S6]: ../../third_party/gpui/crates/gpui/src/element.rs#L1-L184
[S7]: ../../third_party/gpui/crates/gpui/src/view.rs#L285-L481
[S8]: ../../third_party/gpui/crates/gpui/src/window.rs#L2719-L2970
[S9]: ../../third_party/gpui/crates/gpui/docs/key_dispatch.md#L1-L88
[S10]: ../../third_party/gpui/crates/gpui/src/key_dispatch.rs#L1-L223
[S11]: ../../third_party/gpui/crates/gpui/src/executor.rs#L1-L122
[S12]: ../../third_party/gpui/crates/gpui_platform/src/gpui_platform.rs#L1-L60
[S13]: ../../third_party/gpui/crates/gpui/examples/testing.rs#L1-L346
[S14]: ../../third_party/gpui/crates/gpui/src/elements/list.rs#L1-L37
[S15]: ../../third_party/gpui/crates/gpui/src/subscription.rs#L147-L194
[S16]: ../../crates/ui/src/lib.rs#L75-L185
[S17]: ../../third_party/gpui/crates/gpui/Cargo.toml#L1-L42
[S18]: ../../third_party/gpui/crates/gpui/src/app/entity_map.rs#L134-L207
[U1]: https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/gpui/README.md
[U2]: https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/gpui/docs/contexts.md
[U3]: https://gpui.rs/
[U4]: https://github.com/zed-industries/zed/tree/main/crates/gpui
