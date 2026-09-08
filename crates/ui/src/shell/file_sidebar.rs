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
            FileTreeEvent::DiskChanged { paths } => {
                // Live refresh (ticket 04): clean viewers reload from disk;
                // dirty ones enter the conflict state — the notification
                // supplements, never replaces, the save-time check.
                let Some(space) = self.file_space_key(cx) else {
                    return;
                };
                let viewers: Vec<Entity<FileViewer>> = self
                    .file_state
                    .space(&space)
                    .map(|tabs| tabs.tabs.iter().map(|tab| tab.viewer.clone()).collect())
                    .unwrap_or_default();
                for viewer in viewers {
                    let affected = viewer.read(cx).affected_by(paths);
                    if !affected {
                        continue;
                    }
                    viewer.update(cx, |viewer, cx| viewer.on_disk_changed(cx));
                }
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
        // The next preview replaces the previous CLEAN preview — a modified
        // preview has pinned itself on first edit (decision 12).
        let previews: Vec<u64> = self
            .file_state
            .space(&space)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .filter(|tab| !tab.pinned && !tab.viewer.read(cx).is_dirty())
                    .map(|tab| tab.id)
                    .collect()
            })
            .unwrap_or_default();
        self.file_seq += 1;
        let id = self.file_seq;
        let viewer = cx.new(|cx| FileViewer::new(self.state.clone(), path.clone(), scope, cx));
        // Viewer events maintain the tab's bookkeeping: the resolved path
        // (alias identity) and preview pinning on first edit (decision 12).
        let events = {
            let space = space.clone();
            cx.subscribe(
                &viewer,
                move |this: &mut Shell, _, event: &FileViewerEvent, cx| {
                    match event {
                        FileViewerEvent::Loaded { resolved } => {
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
                        }
                        FileViewerEvent::PinRequested => {
                            if let Some(tab) = this
                                .file_state
                                .get(&space)
                                .tabs
                                .iter_mut()
                                .find(|tab| tab.id == id)
                            {
                                tab.pinned = true;
                            }
                        }
                        FileViewerEvent::DirtyChanged { .. } => {}
                        FileViewerEvent::Moved { path } => {
                            let path = path.clone();
                            if let Some(tab) = this
                                .file_state
                                .get(&space)
                                .tabs
                                .iter_mut()
                                .find(|tab| tab.id == id)
                            {
                                tab.path = path;
                                tab.resolved = None;
                            }
                        }
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

    /// Cmd+S: save the file tab the contents area is showing.
    pub(super) fn save_active_file(&mut self, cx: &mut Context<Self>) {
        if let RightSurface::File(id) = self.resolved_right_active(cx)
            && let Some(space) = self.file_space_key(cx)
            && let Some(tab) = self.file_state.space(&space).and_then(|tabs| tabs.find(id))
        {
            let viewer = tab.viewer.clone();
            viewer.update(cx, |viewer, cx| {
                viewer.save(cx);
            });
        }
    }

    /// The modified tabs of one space (strip order).
    fn dirty_tabs(&self, space: &str, cx: &App) -> Vec<u64> {
        self.file_state
            .space(space)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .filter(|tab| tab.viewer.read(cx).is_dirty())
                    .map(|tab| tab.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every modified file tab across all spaces — the quit gate's census.
    pub(crate) fn dirty_file_tabs_everywhere(&self, cx: &App) -> Vec<(String, u64)> {
        let mut dirty = Vec::new();
        for (space, tabs) in self.file_state.spaces() {
            for tab in &tabs.tabs {
                if tab.viewer.read(cx).is_dirty() {
                    dirty.push((space.clone(), tab.id));
                }
            }
        }
        dirty
    }

    /// A close request for a file tab: a clean tab closes now; a modified
    /// one stops for the Save/Discard/Cancel decision (decision 11).
    pub(super) fn request_close_file(
        &mut self,
        id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(space) = self.file_space_key(cx) else {
            return;
        };
        let dirty = self
            .file_state
            .space(&space)
            .and_then(|tabs| tabs.find(id))
            .is_some_and(|tab| tab.viewer.read(cx).is_dirty());
        if dirty {
            self.dirty_file_close = Some((space, id));
            cx.notify();
        } else {
            self.close_file_tab(&space, id, cx);
        }
    }

    /// Close a file tab outright (after a decision or when clean).
    fn close_file_tab(&mut self, space: &str, id: u64, cx: &mut Context<Self>) {
        self.closing_after_save.remove(&id);
        if let Some(viewer) = self.file_state.remove(space, id) {
            self.file_viewers_sub.remove(&id);
            drop(viewer);
        }
        // The chat's stored pick may point at the closed surface;
        // `resolved_right_active` heals it on the next frame.
        let key = self.panel_key(cx);
        self.panels.update(&key, |panels| {
            if panels.right_active == RightSurface::File(id) {
                panels.right_active = RightSurface::Picker;
            }
        });
        cx.notify();
    }

    /// The dirty-close dialog's decision. Save runs the save and closes only
    /// when it succeeds; Discard closes now; Cancel keeps the tab untouched.
    pub(super) fn resolve_dirty_file_close(
        &mut self,
        save: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((space, id)) = self.dirty_file_close.take() else {
            return;
        };
        if !save {
            self.close_file_tab(&space, id, cx);
            return;
        }
        let Some(viewer) = self
            .file_state
            .space(&space)
            .and_then(|tabs| tabs.find(id))
            .map(|tab| tab.viewer.clone())
        else {
            return;
        };
        self.dirty_file_close = None;
        match viewer.update(cx, |viewer, cx| viewer.save(cx)) {
            Some(task) => {
                self.closing_after_save.insert(id);
                let space_for_close = space.clone();
                cx.spawn(async move |this, cx| {
                    let saved = task.await;
                    this.update(cx, |this, cx| {
                        if saved {
                            this.close_file_tab(&space_for_close, id, cx);
                        } else {
                            // A failed save keeps the tab and its draft.
                            this.closing_after_save.remove(&id);
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
            None => {
                // Nothing in flight and nothing to save — close.
                self.close_file_tab(&space, id, cx);
            }
        }
    }

    /// Space removal gate: a space with modified tabs stops for a combined
    /// decision before its records go (ticket 02's multi-file closure).
    pub(super) fn space_removal_needs_draft_decision(&self, space: &str, cx: &App) -> bool {
        !self.dirty_tabs(space, cx).is_empty()
    }

    /// The space-removal decision: Save writes every modified tab (any
    /// failure aborts the removal), Discard drops them, then the space goes.
    pub(super) fn resolve_dirty_space_close(&mut self, save: bool, cx: &mut Context<Self>) {
        let Some(space) = self.dirty_space_close.take() else {
            return;
        };
        if !save {
            self.delete_space(space, cx);
            return;
        }
        let viewers: Vec<Entity<FileViewer>> = self
            .dirty_tabs(&space, cx)
            .into_iter()
            .filter_map(|id| {
                self.file_state
                    .space(&space)
                    .and_then(|tabs| tabs.find(id))
                    .map(|tab| tab.viewer.clone())
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let mut all_saved = true;
            for viewer in viewers {
                let task = viewer.update(cx, |viewer, cx| viewer.save(cx));
                if let Some(task) = task
                    && !task.await
                {
                    all_saved = false;
                    break;
                }
            }
            this.update(cx, |this, cx| {
                if all_saved {
                    this.delete_space(space, cx);
                } else {
                    this.push_holt_notice(
                        HoltNoticeKind::Error,
                        "Could not save every modified file — the space was kept.".into(),
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    /// Save every modified tab across all spaces; completes when every save
    /// settled. `Ok(())` when all saved, `Err(count)` with the failure
    /// count — the quit gate refuses to lose drafts either way.
    pub(crate) fn save_all_dirty_files(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), usize>> {
        let dirty = self.dirty_file_tabs_everywhere(cx);
        let viewers: Vec<_> = dirty
            .into_iter()
            .filter_map(|(space, id)| {
                self.file_state
                    .space(&space)
                    .and_then(|tabs| tabs.find(id))
                    .map(|tab| tab.viewer.clone())
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let mut failures = 0usize;
            for viewer in viewers {
                let task = viewer.update(cx, |viewer, cx| viewer.save(cx));
                if let Some(task) = task
                    && !task.await
                {
                    failures += 1;
                }
            }
            let _ = this.update(cx, |_, cx| cx.notify());
            if failures == 0 { Ok(()) } else { Err(failures) }
        })
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

impl Shell {
    /// The unsaved-file decision dialogs: one modified tab closing, and one
    /// space whose removal would take modified tabs with it. Both offer
    /// Save / Discard / Cancel (decision 11); a failed save keeps what it
    /// tried to close.
    pub(super) fn render_file_draft_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays = Vec::new();

        let dirty_tab = self.dirty_file_close.clone().and_then(|(space, id)| {
            self.file_state
                .space(&space)
                .and_then(|tabs| tabs.find(id))
                .map(|tab| (id, tab.title().to_string()))
        });
        if let Some((_id, title)) = dirty_tab {
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.dirty_file_close = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Save changes?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!(
                        "\u{201C}{title}\u{201D} has unsaved changes. Save them before the tab closes?"
                    ),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "file-close-cancel")
                                .id("file-close-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.dirty_file_close = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Discard")
                                .id("file-close-discard")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.resolve_dirty_file_close(false, window, cx);
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Save")
                                .id("file-close-save")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.resolve_dirty_file_close(true, window, cx);
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("file-draft-close-dialog", viewport, card));
        }

        let dirty_space = self.dirty_space_close.clone();
        if let Some(space) = dirty_space {
            let count = self.dirty_tabs(&space, cx).len();
            let name = self
                .state
                .read(cx)
                .space_row(&space)
                .map(|row| row.display_name().to_string())
                .unwrap_or_else(|| "This space".into());
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.dirty_space_close = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Remove space with unsaved files?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!(
                        "{name} has {count} file(s) with unsaved changes. Save them before the space is removed?"
                    ),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "space-draft-cancel")
                                .id("space-draft-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.dirty_space_close = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Discard")
                                .id("space-draft-discard")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_dirty_space_close(false, cx);
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Save all")
                                .id("space-draft-save")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_dirty_space_close(true, cx);
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("file-draft-space-dialog", viewport, card));
        }

        overlays
    }
}
