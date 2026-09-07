//! Chat-owned terminal groups. A pane retains its viewport while its host moves.

use super::pane::TerminalPane;
use crate::{state::AppState, theme::Theme};
use gpui::{
    App, Context, Entity, FocusHandle, IntoElement, KeyBinding, Render, SharedString, Subscription,
    Window, actions, div, prelude::*,
};
use std::collections::HashMap;

pub use super::pane::{TAB_BAR_HEIGHT, clamp_terminal_height, drop_index, slide_offset};

actions!(
    terminal,
    [
        ToggleTerminal,
        ClosePane,
        FindTerminal,
        SplitHorizontal,
        SplitVertical,
        NewTerminal
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-j"
            } else {
                "ctrl-j"
            },
            ToggleTerminal,
            None,
        ),
        KeyBinding::new("cmd-w", ClosePane, Some("Terminal")),
        KeyBinding::new("cmd-f", FindTerminal, Some("Terminal")),
        KeyBinding::new("cmd-d", SplitHorizontal, Some("Terminal")),
        KeyBinding::new("cmd-shift-d", SplitVertical, Some("Terminal")),
        KeyBinding::new("cmd-t", NewTerminal, Some("Terminal")),
    ]);
}

#[derive(Clone)]
enum Layout {
    Pane(u64),
    Split {
        key: u64,
        vertical: bool,
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    fn contains(&self, target: u64) -> bool {
        match self {
            Self::Pane(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    fn ids(&self, ids: &mut Vec<u64>) {
        match self {
            Self::Pane(id) => ids.push(*id),
            Self::Split { first, second, .. } => {
                first.ids(ids);
                second.ids(ids);
            }
        }
    }
    fn split(&mut self, target: u64, new: u64, vertical: bool) {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    key: new,
                    vertical,
                    ratio: 0.5,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(new)),
                };
            }
            Self::Split { first, second, .. } => {
                first.split(target, new, vertical);
                second.split(target, new, vertical);
            }
            _ => {}
        }
    }
    fn remove(self, target: u64) -> Option<Self> {
        match self {
            Self::Pane(id) => (id != target).then_some(Self::Pane(id)),
            Self::Split {
                key,
                vertical,
                ratio,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    key,
                    vertical,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (first, second) => first.or(second),
            },
        }
    }
    fn resize(&mut self, target: u64, value: f32) {
        if let Self::Split {
            key,
            ratio,
            first,
            second,
            ..
        } = self
        {
            if *key == target {
                *ratio = value.clamp(0.15, 0.85);
            } else {
                first.resize(target, value);
                second.resize(target, value);
            }
        }
    }
}

struct Group {
    key: u64,
    layout: Layout,
    active: u64,
    /// Stable fallback tab number — the group shows as "Terminal N" until
    /// the running program sets an OSC title. Survivors never renumber on
    /// close; freed numbers are reused so names stay unique and dense.
    no: u64,
}
#[derive(Default)]
struct ChatTabs {
    groups: Vec<Group>,
    active: usize,
}

impl ChatTabs {
    /// Lowest tab number not claimed by a live group.
    fn free_no(&self) -> u64 {
        let used: std::collections::HashSet<u64> = self.groups.iter().map(|g| g.no).collect();
        (1..)
            .find(|no| !used.contains(no))
            .expect("a finite set of groups leaves a free u64")
    }
}

struct GroupDrag {
    key: u64,
    title: SharedString,
}

impl Render for GroupDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(gpui::px(112.))
            .h_6()
            .px_2()
            .flex()
            .items_center()
            .bg(Theme::of(cx).surface_raised)
            .text_color(Theme::of(cx).text)
            .text_xs()
            .child(div().truncate().child(self.title.clone()))
    }
}
struct PaneEntry {
    view: Entity<TerminalPane>,
    _observe: Subscription,
    _focus: Subscription,
}

pub struct TerminalPanel {
    state: Entity<AppState>,
    chats: HashMap<String, ChatTabs>,
    panes: HashMap<u64, PaneEntry>,
    seq: u64,
    embedded: bool,
    resize_suspended: bool,
    focus: FocusHandle,
    selected: Option<String>,
    confirming: bool,
    drag: Option<(u64, bool, gpui::Bounds<gpui::Pixels>)>,
    split_bounds: HashMap<u64, gpui::Bounds<gpui::Pixels>>,
    _observe: Subscription,
}

impl TerminalPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| {
            let valid: std::collections::HashSet<_> = this
                .state
                .read(cx)
                .chats
                .iter()
                .map(|chat| chat.id.clone())
                .collect();
            let removed: Vec<_> = this
                .chats
                .keys()
                .filter(|chat| !valid.contains(*chat))
                .cloned()
                .collect();
            for chat in removed {
                if let Some(tabs) = this.chats.remove(&chat) {
                    for group in tabs.groups {
                        let mut ids = Vec::new();
                        group.layout.ids(&mut ids);
                        for id in ids {
                            if let Some(entry) = this.panes.remove(&id) {
                                entry.view.update(cx, |p, cx| p.close_session(cx));
                            }
                        }
                    }
                }
            }
            let selected = this.state.read(cx).selected_chat.clone();
            if selected != this.selected {
                this.selected = selected;
                this.sync_focus(cx);
                cx.notify();
            }
        });
        Self {
            state,
            chats: HashMap::new(),
            panes: HashMap::new(),
            seq: 0,
            embedded: false,
            resize_suspended: false,
            focus: cx.focus_handle(),
            selected: None,
            confirming: false,
            drag: None,
            split_bounds: HashMap::new(),
            _observe: observe,
        }
    }

    fn chat(&self, cx: &App) -> Option<String> {
        self.state.read(cx).selected_chat.clone()
    }
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    pub fn active_key(&self, cx: &App) -> Option<u64> {
        self.active_group(cx).map(|g| g.key)
    }
    pub fn set_embedded(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.embedded != value {
            self.embedded = value;
            cx.notify();
        }
    }
    pub fn set_resize_suspended(&mut self, suspended: bool) {
        self.resize_suspended = suspended;
    }
    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if open {
            self.ensure_tab(cx);
        }
        self.sync_focus(cx);
        cx.notify();
    }
    fn sync_focus(&mut self, cx: &App) {
        if let Some(pane) = self.active_pane(cx) {
            self.focus = pane.read(cx).focus_handle();
        }
    }
    fn active_group(&self, cx: &App) -> Option<&Group> {
        let tabs = self.chats.get(&self.chat(cx)?)?;
        tabs.groups.get(tabs.active)
    }
    fn active_pane(&self, cx: &App) -> Option<Entity<TerminalPane>> {
        self.panes
            .get(&self.active_group(cx)?.active)
            .map(|p| p.view.clone())
    }
    fn ensure_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(chat) = self.chat(cx)
            && self.chats.get(&chat).is_none_or(|t| t.groups.is_empty())
        {
            self.open_tab_for_selected(cx);
        }
    }
    fn activate_pane(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(chat) = self.chat(cx)
            && let Some(tabs) = self.chats.get_mut(&chat)
            && let Some(group) = tabs.groups.get_mut(tabs.active)
            && group.layout.contains(id)
        {
            group.active = id;
            self.sync_focus(cx);
            cx.notify();
        }
    }
    fn create_pane(&mut self, chat: String, cx: &mut Context<Self>) -> u64 {
        self.seq += 1;
        let id = self.seq;
        let view = cx.new(|cx| TerminalPane::new_embedded(self.state.clone(), chat, cx));
        let observe = cx.observe(&view, |_, _, cx| cx.notify());
        let focus = cx.subscribe(&view, move |this, _, _: &super::pane::PaneFocused, cx| {
            this.activate_pane(id, cx);
        });
        self.panes.insert(
            id,
            PaneEntry {
                view,
                _observe: observe,
                _focus: focus,
            },
        );
        id
    }
    pub fn open_tab_for_selected(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        let chat = self.chat(cx)?;
        self.state.read(cx).engine()?;
        let id = self.create_pane(chat.clone(), cx);
        let tabs = self.chats.entry(chat).or_default();
        let no = tabs.free_no();
        tabs.groups.push(Group {
            key: id,
            layout: Layout::Pane(id),
            active: id,
            no,
        });
        tabs.active = tabs.groups.len() - 1;
        self.sync_focus(cx);
        cx.notify();
        Some(id)
    }
    pub fn tab_summaries(&self, cx: &App) -> Vec<(u64, SharedString, bool)> {
        self.chat(cx)
            .and_then(|chat| self.chats.get(&chat))
            .map(|tabs| {
                tabs.groups
                    .iter()
                    .map(|g| {
                        let pane = self.panes.get(&g.active).map(|p| p.view.read(cx));
                        let mut ids = Vec::new();
                        g.layout.ids(&mut ids);
                        let exited = ids.iter().all(|id| {
                            self.panes
                                .get(id)
                                .is_none_or(|p| !p.view.read(cx).running(cx))
                        });
                        (
                            g.key,
                            // The program's OSC title wins (the contextual
                            // name, user request); else this group's stable
                            // "Terminal N" — unique among the chat's tabs.
                            pane.and_then(|p| p.osc_title(cx))
                                .unwrap_or_else(|| format!("Terminal {}", g.no).into()),
                            exited,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn select_tab_by_key(&mut self, key: u64, cx: &mut Context<Self>) {
        if self.active_key(cx) == Some(key) {
            return;
        }
        if let Some(chat) = self.chat(cx)
            && let Some(tabs) = self.chats.get_mut(&chat)
            && let Some(ix) = tabs.groups.iter().position(|g| g.key == key)
        {
            tabs.active = ix;
        }
        self.sync_focus(cx);
        cx.notify();
    }
    fn reorder_tab(&mut self, source: u64, target: u64, cx: &mut Context<Self>) {
        if source == target {
            return;
        }
        if let Some(chat) = self.chat(cx)
            && let Some(tabs) = self.chats.get_mut(&chat)
            && let Some(from) = tabs.groups.iter().position(|g| g.key == source)
            && let Some(to) = tabs.groups.iter().position(|g| g.key == target)
        {
            let active = tabs.groups[tabs.active].key;
            let group = tabs.groups.remove(from);
            tabs.groups.insert(to, group);
            tabs.active = tabs.groups.iter().position(|g| g.key == active).unwrap();
            cx.notify();
        }
    }
    fn split(&mut self, vertical: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.chat(cx) else { return };
        let Some(target) = self.active_group(cx).map(|g| g.active) else {
            return;
        };
        let id = self.create_pane(chat.clone(), cx);
        let tabs = self.chats.get_mut(&chat).unwrap();
        let group = &mut tabs.groups[tabs.active];
        group.layout.split(target, id, vertical);
        group.active = id;
        self.sync_focus(cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }
    pub fn close_tab_by_key(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.request_close(key, None, window, cx);
    }
    fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(g) = self.active_group(cx) {
            self.request_close(g.key, Some(g.active), window, cx);
        }
    }
    fn request_close(
        &mut self,
        key: u64,
        pane: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming {
            return;
        }
        let Some(chat) = self.chat(cx) else { return };
        let Some(group) = self
            .chats
            .get(&chat)
            .and_then(|t| t.groups.iter().find(|g| g.key == key))
        else {
            return;
        };
        let mut ids = Vec::new();
        group.layout.ids(&mut ids);
        if let Some(pane) = pane {
            ids.retain(|id| *id == pane);
        }
        let running = ids
            .iter()
            .filter(|id| {
                self.panes
                    .get(id)
                    .is_some_and(|p| p.view.read(cx).running(cx))
            })
            .count();
        if running == 0 {
            self.remove_panes(&chat, key, &ids, window, cx);
            return;
        }
        self.confirming = true;
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Close running terminals?",
            Some(&format!(
                "This will end {running} terminal(s) and their running programs."
            )),
            &["Cancel", "Close terminals"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let confirmed = answer.await == Ok(1);
            let _ = this.update_in(cx, |this, window, cx| {
                this.confirming = false;
                if confirmed {
                    this.remove_panes(&chat, key, &ids, window, cx);
                }
            });
        })
        .detach();
    }
    fn remove_panes(
        &mut self,
        chat: &str,
        key: u64,
        ids: &[u64],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for id in ids {
            if let Some(entry) = self.panes.remove(id) {
                entry.view.update(cx, |p, cx| p.close_session(cx));
            }
        }
        if let Some(tabs) = self.chats.get_mut(chat)
            && let Some(ix) = tabs.groups.iter().position(|g| g.key == key)
        {
            let mut layout = Some(tabs.groups[ix].layout.clone());
            for id in ids {
                layout = layout.and_then(|l| l.remove(*id));
            }
            if let Some(layout) = layout {
                let mut remaining = Vec::new();
                layout.ids(&mut remaining);
                tabs.groups[ix].layout = layout;
                if ids.contains(&tabs.groups[ix].active) {
                    tabs.groups[ix].active = remaining[0];
                }
            } else {
                tabs.groups.remove(ix);
            }
            tabs.active = tabs.active.min(tabs.groups.len().saturating_sub(1));
        }
        self.sync_focus(cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn render_layout(
        &mut self,
        layout: &Layout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match layout {
            Layout::Pane(id) => {
                let id = *id;
                let view = self.panes[&id].view.clone();
                view.update(cx, |pane, _| {
                    pane.set_resize_suspended(self.resize_suspended)
                });
                let focused = view.read(cx).focus_handle().is_focused(window);
                let theme = Theme::of(cx).clone();
                div()
                    .id(("terminal-pane", id))
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(if focused {
                        theme.border_strong
                    } else {
                        theme.border
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.activate_pane(id, cx);
                        }),
                    )
                    .child(view)
                    .into_any_element()
            }
            Layout::Split {
                key,
                vertical,
                ratio,
                first,
                second,
            } => {
                let (key, vertical, ratio) = (*key, *vertical, *ratio);
                let first = self.render_layout(first, window, cx);
                let second = self.render_layout(second, window, cx);
                let handle = div()
                    .id(("terminal-split-handle", key))
                    .flex_none()
                    .when(vertical, |d| d.h_1().w_full().cursor_row_resize())
                    .when(!vertical, |d| d.w_1().h_full().cursor_col_resize())
                    .bg(Theme::of(cx).border)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(bounds) = this.split_bounds.get(&key) {
                                this.drag = Some((key, vertical, *bounds));
                            }
                            cx.stop_propagation();
                        }),
                    );
                let entity = cx.entity().downgrade();
                div()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .when(vertical, |d| d.flex_col())
                    .child(
                        gpui::canvas(
                            move |bounds, _, cx| {
                                let _ = entity.update(cx, |this, _| {
                                    this.split_bounds.insert(key, bounds);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .flex_grow(ratio)
                            .flex_basis(gpui::px(0.))
                            .child(first),
                    )
                    .child(handle)
                    .child(
                        div()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .flex_grow(1. - ratio)
                            .flex_basis(gpui::px(0.))
                            .child(second),
                    )
                    .into_any_element()
            }
        }
    }
}

pub(super) fn tool(
    id: &'static str,
    icon: &'static str,
    title: &'static str,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = Theme::of(cx);
    div()
        .id(id)
        .size_7()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .hover(|s| s.bg(theme.element_hover))
        .tooltip(move |_, cx| {
            cx.new(|_| crate::image_viewer::ViewerTooltip(title.into()))
                .into()
        })
        .child(
            crate::icons::icon(icon)
                .size_4()
                .text_color(theme.text_muted),
        )
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let active = self.active_group(cx).map(|g| g.key);
        let layout = self.active_group(cx).map(|g| g.layout.clone());
        let rows = self.tab_summaries(cx);
        let bar = div()
            .h_8()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .child(
                div()
                    .id("terminal-tabs")
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .overflow_x_scroll()
                    .children(rows.into_iter().filter(|_| !self.embedded).map(
                        |(key, title, exited)| {
                            div()
                                .id(("terminal-tab", key))
                                .h_6()
                                .w(gpui::px(112.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_2()
                                .rounded_sm()
                                .text_xs()
                                .bg(if Some(key) == active {
                                    theme.element_hover
                                } else {
                                    gpui::transparent_black()
                                })
                                .text_color(if exited { theme.text_faint } else { theme.text })
                                .on_drag(
                                    GroupDrag {
                                        key,
                                        title: title.clone(),
                                    },
                                    |payload, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.new(|_| GroupDrag {
                                            key: payload.key,
                                            title: payload.title.clone(),
                                        })
                                    },
                                )
                                .drag_over::<GroupDrag>(|style, _, _, cx| {
                                    style.bg(Theme::of(cx).element_hover)
                                })
                                .on_drop::<GroupDrag>(cx.listener(
                                    move |this, payload: &GroupDrag, _, cx| {
                                        this.reorder_tab(payload.key, key, cx);
                                    },
                                ))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.select_tab_by_key(key, cx);
                                    window.focus(&this.focus, cx);
                                }))
                                .on_mouse_down(
                                    gpui::MouseButton::Middle,
                                    cx.listener(move |this, _, window, cx| {
                                        this.close_tab_by_key(key, window, cx);
                                        cx.stop_propagation();
                                    }),
                                )
                                .child(div().flex_1().min_w_0().truncate().child(title))
                                .child(
                                    div()
                                        .id("close-terminal-tab")
                                        .size_5()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .text_color(theme.text_muted)
                                        .hover(|s| s.text_color(theme.text))
                                        .tooltip(move |_, cx| {
                                            cx.new(|_| {
                                                crate::image_viewer::ViewerTooltip(
                                                    "Close terminal group".into(),
                                                )
                                            })
                                            .into()
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.close_tab_by_key(key, window, cx);
                                        }))
                                        .child(
                                            crate::icons::icon(crate::icons::CLOSE)
                                                .size_3()
                                                .text_color(theme.text_muted),
                                        ),
                                )
                        },
                    )),
            )
            .child(
                tool("new-terminal", crate::icons::PLUS, "New terminal", cx).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.open_tab_for_selected(cx);
                        window.focus(&this.focus, cx);
                    },
                )),
            )
            .children((!self.embedded).then(|| {
                tool("close-terminal", crate::icons::CLOSE, "Close terminal", cx)
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleTerminal), cx))
            }));
        let body = layout
            .map(|l| self.render_layout(&l, window, cx))
            .unwrap_or_else(|| {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.text_muted)
                    .child(if self.chat(cx).is_some() {
                        "No terminals"
                    } else {
                        "Select a chat"
                    })
                    .into_any_element()
            });
        div()
            .key_context("Terminal")
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .text_sm()
            .on_action(cx.listener(|this, _: &ClosePane, window, cx| this.close_active(window, cx)))
            .on_action(cx.listener(|this, _: &FindTerminal, window, cx| {
                if let Some(pane) = this.active_pane(cx) {
                    pane.update(cx, |p, cx| p.open_search(window, cx));
                }
            }))
            .on_action(
                cx.listener(|this, _: &SplitHorizontal, window, cx| this.split(false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SplitVertical, window, cx| this.split(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &NewTerminal, window, cx| {
                this.open_tab_for_selected(cx);
                window.focus(&this.focus, cx);
            }))
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if let Some((key, vertical, bounds)) = this.drag {
                    if !event.dragging() {
                        this.drag = None;
                        return;
                    }
                    let ratio = if vertical {
                        f32::from(event.position.y - bounds.top()) / f32::from(bounds.size.height)
                    } else {
                        f32::from(event.position.x - bounds.left()) / f32::from(bounds.size.width)
                    };
                    if let Some(chat) = this.chat(cx)
                        && let Some(tabs) = this.chats.get_mut(&chat)
                        && let Some(group) = tabs.groups.get_mut(tabs.active)
                    {
                        group.layout.resize(key, ratio);
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .child(bar)
            .child(div().flex_1().min_h_0().min_w_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_numbers_stay_unique_and_reuse_freed_slots() {
        let mut tabs = ChatTabs::default();
        let group = |key: u64, no: u64| Group {
            key,
            layout: Layout::Pane(key),
            active: key,
            no,
        };
        assert_eq!(tabs.free_no(), 1);
        tabs.groups.push(group(1, 1));
        assert_eq!(tabs.free_no(), 2);
        tabs.groups.push(group(2, 2));
        assert_eq!(tabs.free_no(), 3);
        // Closing Terminal 1 frees its number for the next group while
        // Terminal 2 keeps its name — no duplicate "Terminal 2".
        tabs.groups.retain(|g| g.key != 1);
        assert_eq!(tabs.free_no(), 1);
    }

    #[test]
    fn split_removal_preserves_other_sessions_and_collapses_empty_branches() {
        let mut layout = Layout::Pane(1);
        layout.split(1, 2, false);
        layout.split(2, 3, true);
        let mut ids = Vec::new();
        layout.ids(&mut ids);
        assert_eq!(ids, [1, 2, 3]);
        let layout = layout.remove(2).unwrap();
        let mut ids = Vec::new();
        layout.ids(&mut ids);
        assert_eq!(ids, [1, 3]);
        let layout = layout.remove(1).unwrap();
        assert!(matches!(layout, Layout::Pane(3)));
        assert!(layout.remove(3).is_none());
    }
}
