//! Shell wiring for the File sidebar (ADR-0020): the far-right tree column,
//! the Space-owned file tabs' open/select/close flow, and the pane's width
//! chrome. Child of `shell` so it renders straight off `Shell`'s state.

use super::*;
use crate::files::viewer::FileViewer;

/// Drag marker for the File-tree column resize handle.
pub(super) struct FileTreeResize;

impl Shell {
    /// The space key file state is keyed by for the current selection
    /// (ADR-0020): the selected chat's space, else the selected space.
    pub(super) fn file_space_key(&self, cx: &App) -> Option<String> {
        crate::files::FileStateMap::space_key(self.state.read(cx))
    }

    /// The space's file tabs as strip rows (title-only; the strip adds icons).
    pub(super) fn file_state_row(&self, space: &str) -> Option<Vec<(RightSurface, SharedString)>> {
        self.file_state.space(space).map(|tabs| {
            tabs.tabs
                .iter()
                .map(|tab| (RightSurface::File(tab.id), tab.title().into()))
                .collect()
        })
    }

    /// Strip drags key by chat panel key, file-tab drags by their space —
    /// accept whichever the current selection matches.
    pub(super) fn strip_drag_key_matches(&self, key: &str, cx: &App) -> bool {
        key == self.panel_key(cx)
            || self
                .file_space_key(cx)
                .is_some_and(|space| key == format!("file-space:{space}"))
    }

    /// The File tree panel (lazy: no entity — and no listings — until the
    /// column first renders).
    pub(super) fn file_tree_panel(&mut self, cx: &mut Context<Self>) -> Entity<FileTreePanel> {
        if let Some(tree) = &self.file_tree {
            return tree.clone();
        }
        let tree = cx.new(|cx| FileTreePanel::new(self.state.clone(), cx));
        let events = cx.subscribe(&tree, Self::on_file_tree_event);
        self._file_tree_events = Some(events);
        self.file_tree = Some(tree.clone());
        tree
    }

    fn on_file_tree_event(
        &mut self,
        _: Entity<FileTreePanel>,
        event: &FileTreeEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            FileTreeEvent::OpenFile {
                path,
                resolved,
                pin,
            } => {
                self.open_file(path.clone(), resolved.clone(), *pin, cx);
            }
        }
    }

    /// Open a file into the contents area (decision 12): a duplicate open of
    /// the same resolved path selects its existing tab (alias-aware); a
    /// single-click preview replaces the previous preview; double-click pins.
    pub(super) fn open_file(
        &mut self,
        path: String,
        resolved_hint: Option<String>,
        pin: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(space) = self.file_space_key(cx) else {
            return;
        };
        // The read binds to the OWNING chat/space at open time, never to
        // whichever chat is selected when the reply lands.
        let scope = {
            let state = self.state.read(cx);
            state
                .selected_chat_row()
                .map(|chat| FileScope {
                    chat_id: Some(chat.id.clone()),
                    space_id: None,
                })
                .unwrap_or_else(|| FileScope {
                    chat_id: None,
                    space_id: state.selected_space.clone(),
                })
        };
        // Duplicate open → select (and pin, if this was the double click).
        if let Some(id) = self
            .file_state
            .space(&space)
            .and_then(|tabs| tabs.find_by_path(&path, resolved_hint.as_deref()))
        {
            if pin
                && let Some(tab) = self
                    .file_state
                    .get(&space)
                    .tabs
                    .iter_mut()
                    .find(|t| t.id == id)
            {
                tab.pinned = true;
            }
            self.reveal_contents_pane(cx);
            self.set_right_active(RightSurface::File(id), cx);
            return;
        }
        // The next preview replaces the previous one. Ticket 02 will guard
        // modified previews — an edit pins a tab (decision 12).
        let previews: Vec<u64> = self
            .file_state
            .space(&space)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .filter(|tab| !tab.pinned)
                    .map(|tab| tab.id)
                    .collect()
            })
            .unwrap_or_default();
        self.file_seq += 1;
        let id = self.file_seq;
        let viewer = cx.new(|cx| FileViewer::new(self.state.clone(), path.clone(), scope, cx));
        // The Loaded reply records the engine-resolved path on the tab —
        // future opens through an inside-root alias select this tab.
        let events = {
            let space = space.clone();
            cx.subscribe(
                &viewer,
                move |this: &mut Shell, _, event: &FileViewerEvent, cx| {
                    let FileViewerEvent::Loaded { resolved } = event;
                    let resolved = resolved.clone();
                    if let Some(tab) = this
                        .file_state
                        .get(&space)
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.id == id)
                    {
                        tab.resolved = Some(resolved);
                    }
                    cx.notify();
                },
            )
        };
        self.file_state
            .get(&space)
            .tabs
            .push(crate::files::FileTab {
                id,
                path,
                resolved: resolved_hint,
                pinned: pin,
                viewer,
            });
        self.file_viewers_sub.insert(id, events);
        for preview in previews {
            self.file_state.remove(&space, preview);
        }
        self.reveal_contents_pane(cx);
        self.set_right_active(RightSurface::File(id), cx);
    }

    /// Opening a file reveals the contents area (decision 12); the contents
    /// and tree columns hide and reopen independently of each other.
    fn reveal_contents_pane(&mut self, cx: &mut Context<Self>) {
        if !self.right_pane_open(cx) {
            self.toggle_right_pane(cx);
        }
    }

    /// Width budget for the tree column: the space left of the conversation
    /// floor after the right pane's own minimum. Below the tree's floor the
    /// tree collapses entirely (decision 17) — the titlebar toggle reopens.
    pub(super) fn file_tree_available(&self, cx: &App) -> f32 {
        let sidebar = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let right_reservation = if self.right_pane_open(cx) {
            RIGHT_PANE_MIN
        } else {
            0.0
        };
        (self.viewport_width - sidebar - CHAT_PANEL_MIN - right_reservation).max(0.0)
    }

    pub(super) fn file_tree_target(&self, cx: &App) -> f32 {
        if !self.file_tree_visible {
            return 0.0;
        }
        let available = self.file_tree_available(cx);
        if available < FILE_TREE_MIN {
            return 0.0;
        }
        self.settings.file_tree_width.min(available)
    }

    /// Show/hide the far-right File tree column (its own toggle — independent
    /// of the contents pane per decision 2).
    pub(super) fn toggle_file_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.file_tree_target(cx);
        self.file_tree_visible = !self.file_tree_visible;
        let to = self.file_tree_target(cx);
        self.file_tree_tween = Some(WidthTween::new(from, to));
        if self.file_tree_visible {
            // Landing focus in the tree makes its keyboard navigation live.
            let tree = self.file_tree_panel(cx);
            window.focus(&tree.read(cx).focus_handle(cx), cx);
        }
        cx.notify();
    }

    pub(super) fn on_file_tree_drag(
        &mut self,
        event: &gpui::DragMoveEvent<FileTreeResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        let max = self.file_tree_available(cx);
        self.settings.file_tree_width = if max >= FILE_TREE_MIN {
            width.clamp(FILE_TREE_MIN, max)
        } else {
            FILE_TREE_DEFAULT
        };
        self.file_tree_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    /// The far-right column: full-height glass-friendly panel with a left
    /// hairline, its width clipped through the open/close tween.
    pub(super) fn render_file_tree_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let panel_bg = if theme.is_glass() {
            bg.opacity(0.4)
        } else {
            bg
        };
        let tree = self.file_tree_panel(cx);
        let content = tree.update(cx, |tree, cx| tree.render_panel(cx));
        let panel = div()
            .size_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(panel_bg)
            .overflow_hidden()
            // The titlebar overlays the full-height column; content starts
            // below it (the right pane's own convention).
            .pt(px(Theme::TITLEBAR_HEIGHT))
            .child(content);
        let target = self.file_tree_target(cx);
        self.pane_container(
            self.file_tree_tween,
            target,
            div().h_full().relative().child(panel).into_any_element(),
        )
    }
}
