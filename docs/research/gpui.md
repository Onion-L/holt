# GPUI 原理、用法与最佳实践

> 调研日期：2026-08-30。本文以 Holt 仓库中的 vendored GPUI 快照为 API 真相，面向已经熟悉 Rust 和常见前端状态管理的工程师。

## 1. 结论先行

GPUI 是一个 Rust 原生、GPU 加速、混合 immediate/retained mode 的 UI 框架。它不是 DOM 框架，也不把长期业务状态放进组件树：

- `App` 是运行时和状态所有者；`Entity<T>` 是受 `App` 管理、需要上下文才能读写的强类型句柄。
- `Entity<T: Render>` 是 view。view 被标脏后，`Render::render` 重新声明元素树；未变的 view 可以复用上一帧的 prepaint/paint 数据。
- element 是单帧对象，按 `request_layout -> prepaint -> paint` 工作；跨帧状态依靠 entity，或依靠稳定 `ElementId` 对应的 element state。
- 输入首先形成 hitbox、focus path 和 dispatch tree；业务操作应建模为 `Action`，再由按键、菜单或鼠标触发同一 action/方法。
- `cx.spawn` 跑前台本地 future，`background_executor().spawn` 跑 `Send` 后台 future。`Task` 被丢弃就取消，只有明确需要独立存活时才 `detach()`。
- 当前 crate 仍是 pre-1.0，Holt 使用的是带补丁的冻结快照，不应根据网上最新示例猜 API。

以上结论由 GPUI 自身 README、ownership 文档和本地实现共同支持。[S1][S2][S3][S6][S7][S11] Holt 的快照属性见仓库架构说明。[S0]

## 2. 版本与资料边界

本地 `gpui` crate 声明版本为 `0.2.2`，默认特性包含 `font-kit`、Wayland、X11 和 Windows manifest；测试、benchmark、profiler 是单独特性。[S17] `ARCHITECTURE.md` 进一步说明 `vendor/gpui` 是 Zed fork 的冻结快照，包含 Holt 依赖的 glass/edge-fade 补丁，不从 git 解析依赖。[S0]

官方 README 明确提示 GPUI 仍在快速开发、版本间经常有破坏性变化。[S1] 因而本文的优先级是：

1. `vendor/gpui` 的源码与示例；
2. Holt 当前调用代码；
3. Zed 官方仓库和 `gpui.rs`，只用于背景与交叉验证。

2026-08-30 检查 Zed 官方 `main`（commit `399258feeaf90ad8a3a208c99221ee87b6452f38`）时，其 GPUI README 和 contexts 文档与本地相应文本一致。[U1][U2] 这不代表所有源码 API 都相同。

## 3. 运行模型与架构

### 3.1 从平台事件循环到一帧

启动链路是：

```text
gpui_platform::application()
  -> 为当前 OS 选择 Platform 实现
  -> Application::run
  -> App::open_window 创建根 view
  -> 平台 request-frame/input 回调
  -> Window::draw
  -> root element: request_layout -> prepaint -> paint
  -> Scene 交给 PlatformWindow::draw
```

`gpui_platform::current_platform` 在 macOS、Windows、Linux/FreeBSD 和 wasm 上选择不同实现；`Platform` 提供事件循环、窗口、文本系统和 executor，`PlatformWindow` 接收输入、frame request，并最终提交 `Scene`。[S12] `Application::run` 只是把 `App` 借给启动回调，然后由平台运行循环驱动。[S5]

`Window::draw` 会先处理失效 entity、清理本帧访问记录，再调用 `draw_roots`。根元素先请求 Taffy 布局，随后 prepaint，最后 paint；完成后 text system 收帧并交换 `rendered_frame` / `next_frame`，最终 `PlatformWindow::draw` 提交 scene。[S8]

三个 element 阶段的职责必须分清：[S6][S8]

| 阶段 | 做什么 | 不应做什么 |
| --- | --- | --- |
| `request_layout` | 声明 Taffy layout node，返回布局期状态 | 依赖最终 bounds；注册只在命中测试后才有效的交互 |
| `prepaint` | 拿到 bounds，建立 hitbox、dispatch/focus 元数据，准备绘制数据 | 随意越过 parent content mask；执行重业务 I/O |
| `paint` | 向 scene 写入图元，注册依赖最终绘制顺序的监听器 | 修改布局；把业务状态藏在单帧 element 内 |

### 3.2 为什么叫 hybrid immediate/retained

“Immediate”体现在：每次 view 真正重绘时，`Render::render` 根据当前 state 重新创建 element tree；该树和注册在其中的回调会在下一帧前被丢弃。[S1][S6]

“Retained”体现在三层：

- `App` 长期持有 entity 数据；`Entity<T>` 的强引用计数决定 entity 何时释放。[S3][S5]
- 带稳定 `ElementId` 的 element 可以在相邻帧访问同一个 element state；hover、clicked、图片状态等内建交互依赖这个机制。[S6]
- `View` 记录其读取过的 entities 以及 prepaint/paint ranges。当 bounds、content mask、text style 未变且 view 未脏时，GPUI 会调用 `reuse_prepaint` / `reuse_paint`，跳过重新 render 和绘制构建。[S7][S8]

因此 `cx.notify()` 不是“立刻全窗口重绘”。它把 entity 标记为变化，窗口记录依赖关系并把相应 view 及其祖先标脏；同一帧前的多次失效可以合并。[S4][S8]

## 4. Entity、Context 与数据流

### 4.1 核心类型

| 类型 | 生命周期/能力 | 典型用途 |
| --- | --- | --- |
| `App` | 应用级可变上下文；持有 entities、globals、windows、executors | 创建 entity/window，访问平台服务，注册全局行为 |
| `Context<T>` | `App` + 当前 entity；可解引用为 `App` | `notify`、`emit`、`observe`、`subscribe`、entity-scoped `spawn` |
| `Entity<T>` | 强类型强句柄；本身不能直接解引用 `T` | 通过 `read` / `update` 访问状态 |
| `WeakEntity<T>` | 不延长 entity 生命周期；`upgrade/update` 可失败 | async callback、长寿命 listener，防止强引用环 |
| `AsyncApp` / `AsyncWindowContext` | 可跨 `await`；访问可能失败 | 前台异步任务回到 UI 状态 |
| `Window` | 窗口状态和绘制/输入服务；不是 context | focus、layout、paint、clipboard、window-scoped API |
| `Global` | 按类型存储在 `App` 的应用级状态 | theme、settings、共享服务，不适合每个 view 的瞬态状态 |

GPUI 官方 contexts 文档给出了这些边界；`Context<T>` 的实现也明确持有 `&mut App` 和当前 `WeakEntity<T>`，并实现 `Deref<Target = App>`。[S2][S4]

### 4.2 读取、更新、通知与事件

基本规则：

```rust
let model = cx.new(|_| Model::default());

let snapshot = model.read(cx).value;

model.update(cx, |model, cx| {
    model.value += 1;
    cx.notify();
});
```

`Entity<T>` 不是 `Rc<RefCell<T>>` 的同义包装。状态实际放在 `App` 的 entity map 中；`read` 只读借用，`update` 暂时 lease 出实体并提供 `Context<T>`。重入地读写同一 entity 会触发 double-lease panic，所以不要在一个 entity 的 `update` 闭包内再次 `read/update` 它自己。[S3][S18]

两种通信语义不要混用：

- `notify` / `observe` 表示“这个 entity 的可观察状态变了”，主要用于失效依赖它的 view。[S3][S4]
- `emit` / `subscribe` 表示“发生了一个带类型的领域事件”；发送方需实现 `EventEmitter<Event>`。[S3][S4]

`Subscription` 被 drop 就取消；把它放进拥有监听关系的 struct 字段，生命周期最清晰。`detach()` 会让监听持续到相关 entity 被释放，适合真正与双方 entity 同寿命的关系，不适合为了消除 `must_use` 警告随手调用。[S15]

### 4.3 推荐的状态分层

这是基于上述所有权和失效机制的工程建议：

- 领域状态、异步结果、跨 view 共享状态放 entity；纯派生值尽量由纯函数计算。
- theme、settings、进程级服务放 `Global`，但不要把所有 UI 状态塞进一个可变 global。
- 只属于一个 view 的 focus、scroll、选中项和进行中的 `Task` 放该 view/entity。
- 纯展示组件用 `RenderOnce`，不要为每个 label、badge 都创建 entity。
- 跨层通知优先 typed event；如果接收方只是需要重渲染，使用 `observe + notify`。

## 5. View 与 Element 系统

### 5.1 优先级

从高到低选择抽象：

1. `div()`、`text`、`svg`、`img`、`list` 等现成 elements；
2. `#[derive(IntoElement)] + RenderOnce` 构造无长期状态的可复用组件；
3. `Entity<T: Render>` 构造有身份、有状态、需要观察/异步任务的 view；
4. 只有需要自定义布局、文本输入、命中测试或直接画图时才实现 `Element` / 使用 `canvas`。

这是 `element.rs` 模块文档明确给出的推荐路径：大部分业务应组合现成 element，只有需要手动控制 layout/paint 时才写自定义 element。[S6]

### 5.2 稳定 ID 与跨帧状态

交互 element 应给稳定、同级唯一的 `.id(...)`。`Element::id` 会形成层级化 `GlobalElementId`，GPUI 用它访问跨帧 state；同一父 ID 下重复 ID 会让状态对应错误。[S6]

实践上：

- 列表项 ID 使用领域稳定键，例如 `("message", message.id.clone())`，不要使用会随插入变化的下标。
- 条件分支切换不同控件时避免复用相同 ID，除非它们确实是同一逻辑控件。
- hover、active、scroll handle、图片缓存等依赖 element state 时，确保 ID 不随 render 随机变化。

### 5.3 自定义 Element 的边界

自定义 element 的 `RequestLayoutState` 传递布局期数据，`PrepaintState` 传递 bounds/hitbox/shaped text 等绘制期数据。[S6] 推荐保持这两种状态为“本帧计算缓存”，真正业务状态仍由 entity 持有。

自定义 element 可以突破 parent bounds，但必须显式使用 `Window::with_content_mask`；这适用于 popover/overlay，也意味着作者需要自己保证裁剪、z-order 和命中行为。[S6]

## 6. 事件、Action 与焦点

### 6.1 两类事件

- 原始输入：`on_mouse_down`、`on_click`、`on_key_down` 等，适合处理手势和控件局部行为。
- `Action`：应用语义，例如 `Save`、`MoveDown`、`ClosePanel`。它可以由 key binding、菜单或代码派发，适合业务命令。

GPUI 的 key dispatch 设计是 keyboard-first：用 `actions!` 或 `#[gpui::action]` 定义 action，在 element 上 `on_action`，并用 `key_context` 约束 key binding 生效范围。[S9][S10]

### 6.2 Focus path 与传播

可聚焦 view 通常持有 `FocusHandle`、实现 `Focusable`，并在 render tree 中 `.track_focus(&handle)`。只有 handle 出现在当前 dispatch tree 内，围绕该 focus path 注册的 action 才能按预期解析。[S10][S13]

传播分 capture 和 bubble：键盘 capture 从根向焦点，bubble 从焦点向根；鼠标顺序按 z-order 相反处理。普通业务监听通常使用 bubble。[S8] 嵌套 view 的 action 从最深层向上冒泡，因此 editor 级 binding 会优先于 pane/workspace 级 binding。[S10]

注意默认语义不同：一般事件默认继续传播，可 `cx.stop_propagation()`；action 在 bubble 阶段默认停止，需要显式 `cx.propagate()` 才继续到父级或全局 handler。[S5]

### 6.3 推荐写法

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

鼠标按钮可以调用同一个 `increment`，或者派发同一个 `Increment`，不要复制状态变更逻辑。示例接口来自当前快照的 `examples/testing.rs`。[S13]

## 7. 异步任务

### 7.1 前台、后台与 Tokio

| API | 执行位置 | 限制 | 用途 |
| --- | --- | --- | --- |
| `cx.spawn(...)` | GPUI foreground executor / 主线程 | future 可 `!Send`；通过 `AsyncApp` 回到 UI | 协调 UI 状态、等待 channel/timer |
| `cx.background_executor().spawn(future)` 或 `cx.background_spawn(...)` | 后台 scheduler | future/output 必须 `Send` | CPU 工作、线程安全 I/O |
| `gpui_tokio::Tokio::spawn(cx, future)` | Tokio runtime | 遵循 Tokio 要求 | 依赖 Tokio reactor/driver 的库 |

GPUI 的 executor 与平台 event loop 集成；foreground executor 明确是主线程，background executor 把 `Send` future 排入后台。[S1][S11] Holt 当前先 `gpui_tokio::init(cx)`，仅把需要 Tokio runtime 的 engine bootstrap/shutdown 放到 Tokio；runtime-agnostic 的 RPC channel pump 留在 `cx.spawn`。[S16]

### 7.2 Task 生命周期就是取消策略

`Task<T>` 实现 `Future`；drop 会立即取消，`await` 获得结果，`detach()` 才让任务脱离句柄继续运行。[S11] 推荐：

- 可被新请求替代的加载、搜索、补全：保存为 `Option<Task<()>>`；赋新值时旧 task 自动取消。
- 必须与 view 同寿命的循环：也保存 task 字段，view 释放时自动取消。
- 真正 fire-and-forget 且错误已处理的任务才 detach；`Task<Result<...>>` 优先 `detach_and_log_err`。
- async 闭包中使用 `WeakEntity`/`this.update(...)`，接受 view 可能已经释放；不要强行 `unwrap`。

当前 API 的模式如下：[S4][S11][S13]

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

不要在 foreground task 中做长时间同步计算，它仍然占用 UI 线程。将重计算放 background executor，await 结果后再通过 async context 更新 entity。

## 8. Window 与平台层

`Window` 管理根 view、viewport、focus、dispatch tree、hitboxes、scene、文本布局和 platform window。它不是 context，因此 entity 操作仍需同时传 `&mut App`/`Context<T>`。[S2][S8]

`Platform`/`PlatformWindow` 是 OS 边界，覆盖窗口创建、输入、显示器、clipboard、菜单、文件对话框、URL、文本系统、GPU atlas、窗口装饰和生命周期。[S12] 应尽量调用 GPUI 的跨平台 API，而不是在业务 view 中直接调用 Cocoa/Win32/X11。确有差异时：

- 把 `cfg(target_os)` 限制在初始化或很薄的 platform adapter；
- 设置 `WindowOptions` 时逐平台验证 titlebar/decorations 行为；
- 使用 `on_app_quit` 完成有限时长的异步清理；不要在退出阶段再 spawn foreground task，当前实现会拒绝。[S4][S5]

Holt 的 `run_app` 已遵循这一结构：初始化 platform/application 和 globals，创建单个 `AppState`，再以该 entity 打开根 window，同时在 macOS reopen 时复用同一 state。[S16]

## 9. 常用完整骨架（当前 vendored API）

下面骨架直接采用当前 `examples/testing.rs` 和 Holt 启动方式中的 API：[S13][S16]

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

## 10. 列表、缓存与性能

### 10.1 大列表

不同高度的大列表使用 `list(ListState, renderer)`。它缓存行高，离屏行若改变高度，调用方必须通过 `ListState::splice` 或 `reset` 通知；相同高度的行优先 `uniform_list`，接口和计算更简单。[S14]

建议：

- 虚拟化单位应与最常更新/折叠的业务单位对齐；行级 diff 适合 `list`，固定高表格适合 `uniform_list`。
- renderer 只捕获必要的 entity/轻量数据；避免每个可见行 clone 整个大模型。
- 使用稳定 item ID；插入/删除同步更新 list state，不能只改 backing vector。
- 不可见数据的昂贵 parse/shape 放缓存或后台任务，不要依赖“反正 list 不 render”来掩盖上游工作。

### 10.2 避免无效失效

- 只在会影响观察者/渲染的状态确实变化后 `notify`；批量 mutation 后通知一次。
- 将频繁变化的小状态放更小的 child entity，避免根 entity 的一次 notify 让过多 view 变脏。
- `RenderOnce` 组件保持纯；昂贵的纯计算先缓存到 entity，再 render 缓存结果。
- 不要调用全局 `refresh_windows` 代替正确的 entity 通知；它会放弃细粒度复用。
- animation 需要每帧刷新是例外，但仍应限制动画子树和生命周期。

GPUI 会跟踪 view 读取的 entity 并复用未变子树，但这不是忽略数据边界的理由；依赖面越小，dirty view 集合越小。[S7][S8]

### 10.3 测量

本地 crate 暴露 `bench`、`profiler`、`input-latency-histogram` 特性；window 内部记录 dirty-to-draw 时间和合并的 invalidation 数量。[S8][S17] 性能优化应按顺序进行：

1. 用 profiler/benchmark 确认是状态失效、layout、text shaping、paint 还是 GPU present；
2. 缩小 entity 依赖与 list 渲染范围；
3. 缓存纯计算/文本布局输入；
4. 最后才写 custom element 或低层 canvas。

## 11. 测试策略

`#[gpui::test]` 提供 `TestAppContext`；涉及 window/render/focus/action 时，从 window 构造 `VisualTestContext`。同步 side effect 在 `update` 后执行，测试 executor 是单线程，异步或 detached task 要显式 `run_until_parked()`；外部线程/I/O 等 GPUI 无法驱动的 future 默认会暴露停车/死锁问题，确有需要才 `allow_parking()`。[S13]

推荐测试层次：

1. 纯 reducer/排序/派生函数：普通 Rust 单元测试，最快且不依赖 GPUI。
2. entity 数据流：`#[gpui::test]`，验证 `update/notify/emit/subscribe` 和 task 取消。
3. view 行为：`VisualTestContext`，通过 focus handle 派发 action，验证 render-dependent state。
4. 关键自定义 element：测试 layout、hitbox、输入法、selection 边界。
5. 并发/异步状态机：使用多 `TestAppContext`、`iterations` 和受控 dispatcher 探索不同调度顺序。

不要把所有验证都做成截图测试。GPUI 官方示例重点测试 state、action dispatch 和确定性 executor；视觉快照适合少量稳定组件。[S13]

## 12. 反模式清单

| 反模式 | 后果 | 替代方案 |
| --- | --- | --- |
| 根据最新博客复制 API 到 Holt | pre-1.0 API 与 fork 补丁不匹配 | 先查 `vendor/gpui` 和本地 examples |
| 在 `render` 中启动 task、订阅或改 entity | 每次重绘重复 side effect，可能形成循环 | 在 constructor/显式事件方法中创建并保存句柄 |
| mutation 后忘记 `notify` | view 保留旧画面 | 在一次逻辑 mutation 末尾通知一次 |
| 无变化也持续 `notify` | 失去 view cache 和 paint reuse | 比较新旧值或在 reducer 返回 changed flag |
| 所有订阅都 `.detach()` | 无法显式停止，生命周期模糊 | 保存 `Subscription` 字段 |
| 所有 task 都 `.detach()` | view 释放后仍工作，结果/错误无人接收 | 保存/await task；仅真正独立任务 detach |
| async 中强持有 view 或 unwrap update | 延长生命周期或在 window/entity 关闭后 panic | 使用 `WeakEntity`，处理 fallible update |
| foreground future 做 CPU 重活 | 阻塞事件循环和绘制 | background executor 计算，foreground 提交结果 |
| 用数组下标作为动态行 ID | 插入后 hover/focus/element state 串行 | 使用领域稳定键 |
| 普通组件直接实现 `Element` | layout/paint/命中复杂度上升 | `RenderOnce` + 现成 elements |
| 原始 key handler 直接写业务逻辑 | 菜单/快捷键/按钮行为分叉 | 定义 `Action` 或共享 entity method |
| 全局刷新代替依赖通知 | 整窗失效，难定位性能问题 | `cx.notify()` + 小 entity 边界 |

这些替代方案分别来自 element 生命周期、subscription/task drop 语义、key dispatch 和 view reuse 机制。[S6][S7][S9][S11][S15]

## 13. Holt 落地建议

Holt 当前方向总体符合 GPUI 模型：一个根 `AppState` 连接 engine，纯 derivation 放 `holt_proto::view`，GPUI task 负责把 RPC frame 折入 entity；窗口级 view 通过观察 state 重绘。[S16]

后续按优先级建议：

1. **继续坚持 vendored API-first。** 新增 GPUI 用法先在 `vendor/gpui/crates/gpui/examples` 和本地调用点搜索；升级应作为独立工作，记录 fork patch，而不是局部混入上游新 API。[S0][S1]
2. **拆分高频变化的依赖。** `AppState` 很大；如果 profiler 显示根状态通知导致广泛 dirty，优先把高频 transcript、terminal、changes 等拆成 child entity，根 state 只保留句柄和导航选择。是否拆分以 dirty-view 数据为准。[S7][S8][S16]
3. **保持纯 reducer 可脱离 GPUI 测试。** `state.rs` 已明确这么做，应继续让 GPUI glue 只负责 subscription/task/notify。[S16]
4. **系统化 task ownership。** 搜索/加载/parse 等可替换任务保存为字段；应用级 URL pump、注册 scheme 等才 detach。Holt 已有大量 `Option<Task<_>>`，继续统一这一约定。[S11][S16]
5. **明确 runtime 边界。** 依赖 Tokio driver 的 engine/transport 工作走 `gpui_tokio::Tokio::spawn`；普通 channel 协调和 UI 更新走 `cx.spawn`；CPU parse/图片处理走 background executor。[S11][S16]
6. **保留虚拟化约束。** `changes.rs` 的变高行若在离屏状态变化，要同步 `ListState::splice/reset`；固定高的新表格优先 `uniform_list`。[S14]
7. **复用语义 action。** `app_menus.rs`、`shell.rs`、`composer.rs` 已使用 actions/key contexts；新增按钮/菜单时让它们调用同一命令路径，避免快捷键独有逻辑。[S9][S16]
8. **自定义 element 配套低层测试。** `composer.rs`、markdown selection、frost/edge-fade 依赖 request-layout/prepaint/paint 边界；修改时至少覆盖 bounds、focus/action、scroll/selection 和 mask，不只测纯 state。[S6][S8][S13]
9. **订阅默认保存在 owner。** Holt 已普遍使用 `_observe: Subscription` 等字段；保持该模式，只有确定与 entity 同寿命的 app 级监听才 detach。[S15][S16]
10. **把性能开关纳入专项诊断。** 出现卡顿时启用 vendored `profiler`/input latency 能力，记录 invalidation 数和 frame timing，再决定拆 entity、改 list 还是缓存 text/image。[S8][S17]

## 14. 一手资料索引

### 本仓库

- [S0] [`ARCHITECTURE.md:55-62`](../../ARCHITECTURE.md#L55-L62)：vendored fork、冻结快照和 docs 定位。
- [S1] [`vendor/gpui/crates/gpui/README.md:1-98`](../../vendor/gpui/crates/gpui/README.md#L1-L98)：定位、启动方式、entity/view/element 三层、executor 与测试。
- [S2] [`vendor/gpui/crates/gpui/docs/contexts.md:1-33`](../../vendor/gpui/crates/gpui/docs/contexts.md#L1-L33)：各 context、`Window`、`Entity` 的职责。
- [S3] [`vendor/gpui/crates/gpui/src/_ownership_and_data_flow.rs:1-138`](../../vendor/gpui/crates/gpui/src/_ownership_and_data_flow.rs#L1-L138)：所有权、read/update、observe/notify、subscribe/emit。
- [S4] [`vendor/gpui/crates/gpui/src/app/context.rs:20-269`](../../vendor/gpui/crates/gpui/src/app/context.rs#L20-L269)、[`Context::emit:765-779`](../../vendor/gpui/crates/gpui/src/app/context.rs#L765-L779)：entity context、listener、spawn 与事件。
- [S5] [`vendor/gpui/crates/gpui/src/app.rs:225-309`](../../vendor/gpui/crates/gpui/src/app.rs#L225-L309)、[`App::spawn:1847-1895`](../../vendor/gpui/crates/gpui/src/app.rs#L1847-L1895)、[`App::on_action:2145-2185`](../../vendor/gpui/crates/gpui/src/app.rs#L2145-L2185)：运行、executor、传播语义。
- [S6] [`vendor/gpui/crates/gpui/src/element.rs:1-184`](../../vendor/gpui/crates/gpui/src/element.rs#L1-L184)：element 生命周期、自定义 element、`Render`/`RenderOnce`。
- [S7] [`vendor/gpui/crates/gpui/src/view.rs:285-481`](../../vendor/gpui/crates/gpui/src/view.rs#L285-L481)：view cache、dirty 判断、prepaint/paint reuse。
- [S8] [`vendor/gpui/crates/gpui/src/window.rs:88-205`](../../vendor/gpui/crates/gpui/src/window.rs#L88-L205)、[`Window::draw:2719-2868`](../../vendor/gpui/crates/gpui/src/window.rs#L2719-L2868)、[`draw_roots:2893-2970`](../../vendor/gpui/crates/gpui/src/window.rs#L2893-L2970)：传播、失效、帧管线和 scene 提交。
- [S9] [`vendor/gpui/crates/gpui/docs/key_dispatch.md:1-88`](../../vendor/gpui/crates/gpui/docs/key_dispatch.md#L1-L88)：action、key context、binding。
- [S10] [`vendor/gpui/crates/gpui/src/key_dispatch.rs:1-50`](../../vendor/gpui/crates/gpui/src/key_dispatch.rs#L1-L50)、[`DispatchTree:68-223`](../../vendor/gpui/crates/gpui/src/key_dispatch.rs#L68-L223)、[`FocusHandle/Focusable`](../../vendor/gpui/crates/gpui/src/window.rs#L383-L565)：嵌套 action、dispatch tree 与焦点。
- [S11] [`vendor/gpui/crates/gpui/src/executor.rs:1-122`](../../vendor/gpui/crates/gpui/src/executor.rs#L1-L122)、[`vendor/gpui/crates/scheduler/src/executor.rs:178-339`](../../vendor/gpui/crates/scheduler/src/executor.rs#L178-L339)：前后台 executor、`Task` 取消与 detach。
- [S12] [`vendor/gpui/crates/gpui_platform/src/gpui_platform.rs:1-60`](../../vendor/gpui/crates/gpui_platform/src/gpui_platform.rs#L1-L60)、[`Platform`](../../vendor/gpui/crates/gpui/src/platform.rs#L125-L320)、[`PlatformWindow`](../../vendor/gpui/crates/gpui/src/platform.rs#L794-L958)：平台选择与抽象面。
- [S13] [`vendor/gpui/crates/gpui/examples/testing.rs:1-346`](../../vendor/gpui/crates/gpui/examples/testing.rs#L1-L346)：当前 API 的 view/action/async/test 完整示例。
- [S14] [`vendor/gpui/crates/gpui/src/elements/list.rs:1-37`](../../vendor/gpui/crates/gpui/src/elements/list.rs#L1-L37)、[`uniform_list.rs:22-126`](../../vendor/gpui/crates/gpui/src/elements/uniform_list.rs#L22-L126)：不同高度/固定高度列表的约束。
- [S15] [`vendor/gpui/crates/gpui/src/subscription.rs:147-194`](../../vendor/gpui/crates/gpui/src/subscription.rs#L147-L194)：subscription drop/detach 语义。
- [S16] [`crates/ui/src/lib.rs:75-185`](../../crates/ui/src/lib.rs#L75-L185)、[`crates/ui/src/state.rs:1-18`](../../crates/ui/src/state.rs#L1-L18)：Holt 启动、runtime 桥接与 state 分层。
- [S17] [`vendor/gpui/crates/gpui/Cargo.toml:1-42`](../../vendor/gpui/crates/gpui/Cargo.toml#L1-L42)：本地版本和 feature。
- [S18] [`vendor/gpui/crates/gpui/src/app/entity_map.rs:134-207`](../../vendor/gpui/crates/gpui/src/app/entity_map.rs#L134-L207)、[`Entity/WeakEntity:414-800`](../../vendor/gpui/crates/gpui/src/app/entity_map.rs#L414-L800)：entity lease/read、强弱句柄与 double-lease 检查。

### 官方外部资料

- [U1] [Zed GPUI README（固定到 399258f）](https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/gpui/README.md)
- [U2] [Zed GPUI contexts（固定到 399258f）](https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/gpui/docs/contexts.md)
- [U3] [GPUI 官方 API 文档](https://gpui.rs/)
- [U4] [Zed 官方仓库中的 GPUI crate](https://github.com/zed-industries/zed/tree/main/crates/gpui)

[S0]: ../../ARCHITECTURE.md#L55-L62
[S1]: ../../vendor/gpui/crates/gpui/README.md#L1-L98
[S2]: ../../vendor/gpui/crates/gpui/docs/contexts.md#L1-L33
[S3]: ../../vendor/gpui/crates/gpui/src/_ownership_and_data_flow.rs#L1-L138
[S4]: ../../vendor/gpui/crates/gpui/src/app/context.rs#L20-L269
[S5]: ../../vendor/gpui/crates/gpui/src/app.rs#L225-L309
[S6]: ../../vendor/gpui/crates/gpui/src/element.rs#L1-L184
[S7]: ../../vendor/gpui/crates/gpui/src/view.rs#L285-L481
[S8]: ../../vendor/gpui/crates/gpui/src/window.rs#L2719-L2970
[S9]: ../../vendor/gpui/crates/gpui/docs/key_dispatch.md#L1-L88
[S10]: ../../vendor/gpui/crates/gpui/src/key_dispatch.rs#L1-L223
[S11]: ../../vendor/gpui/crates/gpui/src/executor.rs#L1-L122
[S12]: ../../vendor/gpui/crates/gpui_platform/src/gpui_platform.rs#L1-L60
[S13]: ../../vendor/gpui/crates/gpui/examples/testing.rs#L1-L346
[S14]: ../../vendor/gpui/crates/gpui/src/elements/list.rs#L1-L37
[S15]: ../../vendor/gpui/crates/gpui/src/subscription.rs#L147-L194
[S16]: ../../crates/ui/src/lib.rs#L75-L185
[S17]: ../../vendor/gpui/crates/gpui/Cargo.toml#L1-L42
[S18]: ../../vendor/gpui/crates/gpui/src/app/entity_map.rs#L134-L207
[U1]: https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/gpui/README.md
[U2]: https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/gpui/docs/contexts.md
[U3]: https://gpui.rs/
[U4]: https://github.com/zed-industries/zed/tree/main/crates/gpui
