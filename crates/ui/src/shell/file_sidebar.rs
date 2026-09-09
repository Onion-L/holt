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

    /// Snapshot every LIVE Space's file navigation into the persisted
    /// settings record and schedule the debounced save. Called from every
    /// navigation mutation (tab open/close/reorder/pin/select, expansion
    /// changes, space removal). Contents never ride along - a restart
    /// observes disk, it never claims a recovered draft (ticket 05).
    pub(super) fn persist_file_navigation(&mut self, cx: &mut Context<Self>) {
        let tree = self.file_tree.clone();
        let live: Vec<(
            String,
            Vec<crate::settings::PersistedFileTab>,
            Option<usize>,
        )> = self
            .file_state
            .spaces()
            .map(|(space, tabs)| {
                let selected = tabs
                    .tabs
                    .iter()
                    .position(|tab| Some(tab.id) == self.file_state.active(space));
                (
                    space.clone(),
                    tabs.tabs
                        .iter()
                        .map(|tab| crate::settings::PersistedFileTab {
                            path: tab.path.clone(),
                            pinned: tab.pinned,
                        })
                        .collect(),
                    selected,
                )
            })
            .collect();
        for (space, tabs, selected) in live {
            // `cwd:` fallback keys are session-only — never persisted, so
            // no unrestorable record accumulates.
            if space.starts_with("cwd:") {
                self.settings.file_navigation.remove(&space);
                continue;
            }
            let mut record = self
                .settings
                .file_navigation
                .remove(&space)
                .unwrap_or_default();
            record.tabs = tabs;
            record.selected = selected;
            // Expansion follows the tree's live view; a Space the tree has
            // not visited this run keeps its stored expansion.
            if let Some(tree) = &tree
                && let Some(expanded) = tree.read(cx).expanded_for(&space)
            {
                record.expanded = expanded;
            }
            // An empty record (no tabs, no expansion) removes the entry —
            // visited Spaces do not bloat the settings file.
            if record.tabs.is_empty() && record.expanded.is_empty() {
                self.settings.file_navigation.remove(&space);
            } else {
                self.settings.file_navigation.insert(space, record);
            }
        }
        self.schedule_save(cx);
    }

    /// Restore a Space's file navigation from the persisted record, once
    /// per run: tabs come back WITHOUT reads (each read defers until its
    /// tab first renders), the selected tab heals by index, and no draft
    /// state exists. A missing or unreadable path surfaces when its tab is
    /// shown - it never blocks startup or recreates anything.
    pub(super) fn restore_file_navigation_if_needed(
        &mut self,
        space: &str,
        cx: &mut Context<Self>,
    ) {
        if self.file_state.space(space).is_some() {
            return; // already live (or already restored) this run
        }
        // Only real Space identities persist; `cwd:` fallback keys are
        // session-only.
        let record = if space.starts_with("cwd:") {
            crate::settings::SpaceFileNavigation::default()
        } else {
            self.settings
                .file_navigation
                .get(space)
                .cloned()
                .unwrap_or_default()
        };
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
        let state = self.state.clone();
        let mut restored: Vec<(u64, crate::settings::PersistedFileTab, Entity<FileViewer>)> =
            Vec::new();
        for persisted in record.tabs {
            self.file_seq += 1;
            let id = self.file_seq;
            let viewer = cx.new(|cx| {
                FileViewer::restored(state.clone(), persisted.path.clone(), scope.clone(), cx)
            });
            restored.push((id, persisted, viewer));
        }
        let selected_id = record
            .selected
            .and_then(|index| restored.get(index).map(|(id, _, _)| *id));
        {
            let entry = self.file_state.get(space);
            for (id, persisted, viewer) in restored {
                let events = Self::subscribe_viewer(space, id, &viewer, cx);
                self.file_viewers_sub.insert(id, events);
                entry.tabs.push(crate::files::FileTab {
                    id,
                    path: persisted.path,
                    resolved: None,
                    pinned: persisted.pinned,
                    viewer,
                });
            }
        }
        if let Some(id) = selected_id {
            self.file_state.set_active(space, id);
            // Landing pick: a chat that never chose a surface shows the
            // persisted tab when the contents pane opens (the pane itself
            // stays closed until the user opens it).
            let key = self.panel_key(cx);
            self.panels.update(&key, |panels| {
                if panels.right_active == RightSurface::Picker {
                    panels.right_active = RightSurface::File(id);
                }
            });
        }
        cx.notify();
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
        // Expansion and selection changes ride the panel's notifies into
        // the persisted navigation record (debounced).
        let expansion = cx.observe(&tree, |this: &mut Shell, _, cx| {
            this.persist_file_navigation(cx);
        });
        self._file_tree_expansion = Some(expansion);
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
            FileTreeEvent::ContextMenu { target } => {
                let Some(space) = self.file_space_key(cx) else {
                    return;
                };
                let mut menu = popover::Popup::default();
                menu.open((target.clone(), space));
                self.file_menu = menu;
                cx.notify();
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
        let events = Self::subscribe_viewer(&space, id, &viewer, cx);
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
        self.persist_file_navigation(cx);
    }

    /// The per-viewer event hookup every tab - live opens and navigation
    /// restores alike - carries: resolved-path identity, edit-to-pin, and
    /// Save As moves.
    fn subscribe_viewer(
        space: &str,
        id: u64,
        viewer: &Entity<FileViewer>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let space = space.to_string();
        cx.subscribe(
            viewer,
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
                        this.persist_file_navigation(cx);
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
                        this.persist_file_navigation(cx);
                    }
                }
                cx.notify();
            },
        )
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
    /// tree collapses entirely (decision 17) — the surface picker's File row
    /// reopens it once the width allows (ticket 11: one entry point).
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
        self.persist_file_navigation(cx);
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

    /// The width flip + tween behind the tree column's visibility. Focus (and
    /// therefore `Window`) is the caller's concern.
    pub(super) fn set_file_tree_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        let from = self.file_tree_target(cx);
        self.file_tree_visible = visible;
        let to = self.file_tree_target(cx);
        self.file_tree_tween = Some(WidthTween::new(from, to));
        cx.notify();
    }

    /// Land keyboard focus in the tree so its navigation keys go live.
    pub(super) fn focus_file_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tree = self.file_tree_panel(cx);
        window.focus(&tree.read(cx).focus_handle(cx), cx);
    }

    /// The surface picker's File card / the `+` menu's File row (ticket 11):
    /// the File surface — the far-right tree for the current Space plus its
    /// file tabs in the shared contents area. Reveals the tree column and the
    /// contents pane when either is hidden, and activates the surface (the
    /// Space's live tab, or the pick-a-file empty state). Returns whether the
    /// tree column was newly revealed, so the caller can land focus in it.
    pub(super) fn add_file_surface(&mut self, cx: &mut Context<Self>) -> bool {
        let tree_was_hidden = !self.file_tree_visible;
        if tree_was_hidden {
            self.set_file_tree_visible(true, cx);
        }
        self.reveal_contents_pane(cx);
        self.set_right_active(RightSurface::Files, cx);
        tree_was_hidden
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
    /// hairline, its width clipped through the open/close tween. A compact
    /// search row (ticket 09) heads the column — the find-file palette's
    /// click affordance next to its ⌘P key.
    pub(super) fn render_file_tree_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.file_tree_target(cx);
        let surface = self.render_file_tree_surface(cx);
        self.pane_container(
            self.file_tree_tween,
            target,
            div().h_full().relative().child(surface).into_any_element(),
        )
    }

    /// File browsing surface hosted by the shared right pane.
    pub(super) fn render_file_tree_surface(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let panel_bg = if theme.is_glass() {
            bg.opacity(0.4)
        } else {
            bg
        };
        let tree = self.file_tree_panel(cx);
        let content = tree.update(cx, |tree, cx| tree.render_panel(cx));
        let header = div()
            .id("file-tree-search")
            .h(px(30.0))
            .flex_none()
            .mx(px(8.0))
            .mt(px(6.0))
            .mb(px(2.0))
            .px(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .bg(crate::theme::ink(0.045))
            .border_1()
            .border_color(crate::theme::hairline(0.07))
            .hover(|style| style.bg(crate::theme::ink(0.08)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_file_lookup(cx);
            }))
            .child(
                icon(crate::icons::MAGNIFER)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.8)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child("Search files"),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(4.0))
                    .h(px(16.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .bg(crate::theme::ink(0.05))
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_family(theme.font_mono.clone())
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(crate::settings::badge_combo("mod-p")),
            )
            // The column's own hide control (ticket 11): the top-right
            // file-tree button is gone, so the panel carries its collapse —
            // reopening is the surface picker's File row.
            .child(
                div()
                    .id("hide-file-tree")
                    .flex_none()
                    .size(px(20.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::ink(0.09)))
                    .tooltip(|_, cx| {
                        cx.new(|_| crate::image_viewer::ViewerTooltip("Hide file sidebar".into()))
                            .into()
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_right_pane(cx);
                    }))
                    .child(
                        icon(crate::icons::TREE_SIDEBAR)
                            .size(px(12.0))
                            .text_color(theme.text_muted.opacity(0.8)),
                    ),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(panel_bg)
            .overflow_hidden()
            .child(header)
            .child(content)
            .into_any_element()
    }
}

/// A create/rename dialog in flight (ticket 06).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FileOpKind {
    NewFile { parent: String },
    NewDirectory { parent: String },
    Rename { path: String },
}

pub(super) struct FileOpDialog {
    kind: FileOpKind,
    /// The owning Space, bound when the dialog opened — pending results
    /// apply to it even if the user switches Chats mid-flight.
    space: String,
    input: Entity<crate::composer::ComposerInput>,
    /// Validation or operation error, shown inline. A failure keeps the
    /// dialog (and the name) for correction.
    error: Option<SharedString>,
    /// Suppresses submit while an RPC is in flight.
    pending: bool,
    focus_pending: bool,
    _events: Option<Subscription>,
}

impl Shell {
    /// Open a create/rename dialog from the tree's context menu.
    pub(super) fn open_file_op_dialog(&mut self, kind: FileOpKind, cx: &mut Context<Self>) {
        let Some(space) = self.file_space_key(cx) else {
            return;
        };
        let (title, initial) = match &kind {
            FileOpKind::NewFile { .. } => ("New file", ""),
            FileOpKind::NewDirectory { .. } => ("New directory", ""),
            FileOpKind::Rename { path } => (
                "Rename",
                path.trim_end_matches('/').rsplit('/').next().unwrap_or(""),
            ),
        };
        let _ = title;
        let input =
            cx.new(|cx| crate::composer::ComposerInput::with_context("Name", "PaletteSearch", cx));
        input.update(cx, |input, cx| input.set_text(initial, cx));
        let events = cx.subscribe(
            &input,
            |this: &mut Shell, _, event: &crate::composer::ComposerInputEvent, cx| {
                if matches!(event, crate::composer::ComposerInputEvent::Submitted) {
                    this.submit_file_op(cx);
                }
            },
        );
        self.file_op_dialog = Some(FileOpDialog {
            kind,
            space,
            input,
            error: None,
            pending: false,
            focus_pending: true,
            _events: Some(events),
        });
        cx.notify();
    }

    fn close_file_op_dialog(&mut self, cx: &mut Context<Self>) {
        self.file_op_dialog = None;
        cx.notify();
    }

    /// Client-side name validation — the engine re-validates (its fence is
    /// authoritative); this just fails fast without a round trip.
    fn validate_entry_name(name: &str) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("The name must not be empty.".into());
        }
        if name.contains('/') || name == "." || name == ".." || name == ".git" {
            return Err(format!("{name:?} is not a valid name."));
        }
        Ok(())
    }

    /// Submit the dialog: create the entry or apply the rename. On success
    /// the tree refreshes that directory DIRECTLY (no reliance on the
    /// watch), and a rename carries every affected tab and draft to the new
    /// identity.
    pub(super) fn submit_file_op(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(dialog) = &mut self.file_op_dialog else {
            return;
        };
        if dialog.pending {
            return;
        }
        let name = dialog.input.read(cx).text().trim().to_string();
        if let Err(message) = Self::validate_entry_name(&name) {
            dialog.error = Some(message.into());
            cx.notify();
            return;
        }
        let space = dialog.space.clone();
        let kind = dialog.kind.clone();
        // The scope binds to the OWNING space's selector shape: the dialog
        // opened from the tree, whose selector follows the current chat.
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
                    space_id: Some(space.clone()),
                })
        };
        let mut params = serde_json::Map::new();
        if let Some(chat_id) = &scope.chat_id {
            params.insert("chatId".into(), serde_json::json!(chat_id));
        } else if let Some(space_id) = &scope.space_id {
            params.insert("spaceId".into(), serde_json::json!(space_id));
        }
        let (method, refresh_dir, rename_from): (&str, String, Option<String>) = match &kind {
            FileOpKind::NewFile { parent } => {
                params.insert("parentPath".into(), serde_json::json!(parent));
                params.insert("name".into(), serde_json::json!(name));
                params.insert("isDir".into(), serde_json::json!(false));
                (
                    holt_rpc::methods::CREATE_WORKSPACE_ENTRY,
                    parent.clone(),
                    None,
                )
            }
            FileOpKind::NewDirectory { parent } => {
                params.insert("parentPath".into(), serde_json::json!(parent));
                params.insert("name".into(), serde_json::json!(name));
                params.insert("isDir".into(), serde_json::json!(true));
                (
                    holt_rpc::methods::CREATE_WORKSPACE_ENTRY,
                    parent.clone(),
                    None,
                )
            }
            FileOpKind::Rename { path } => {
                params.insert("path".into(), serde_json::json!(path));
                params.insert("newName".into(), serde_json::json!(name));
                (
                    holt_rpc::methods::RENAME_WORKSPACE_ENTRY,
                    std::path::Path::new(path)
                        .parent()
                        .map(|parent| parent.display().to_string())
                        .unwrap_or_default(),
                    Some(path.clone()),
                )
            }
        };
        dialog.pending = true;
        dialog.error = None;
        self.file_op_epoch += 1;
        let epoch = self.file_op_epoch;
        let params = serde_json::Value::Object(params);
        cx.spawn(async move |this, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                method,
                params,
                std::time::Duration::from_secs(10),
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                // The dialog may have been cancelled or replaced after the
                // RPC fired. A SUCCESSFUL outcome still happened on disk:
                // tabs and the tree follow it either way. Failures surface
                // only where a matching dialog can still show them — a
                // stale one never poisons the current dialog (epoch).
                let same_dialog = this
                    .file_op_dialog
                    .as_ref()
                    .map(|dialog| dialog.space == space && dialog.kind == kind)
                    .unwrap_or(false);
                let current_epoch = this.file_op_epoch;
                if same_dialog && let Some(dialog) = &mut this.file_op_dialog {
                    dialog.pending = false;
                }
                match reply {
                    Ok(value) => {
                        if same_dialog {
                            this.close_file_op_dialog(cx);
                        }
                        if let Some(from) = rename_from {
                            // The engine reports the canonical destination;
                            // the reconstructed hint is only a fallback for
                            // older spellings.
                            let destination = value
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    this.rename_destination_hint(&name, &refresh_dir)
                                });
                            this.carry_tabs_over_rename(&space, &from, &destination, cx);
                            if let Some(tree) = &this.file_tree {
                                tree.update(cx, |tree, _| {
                                    tree.carry_tree_over_rename(&from, &destination);
                                });
                            }
                        }
                        this.refresh_tree_dir(&refresh_dir, cx);
                        this.persist_file_navigation(cx);
                    }
                    Err(message) => {
                        if same_dialog && current_epoch == epoch {
                            if let Some(dialog) = &mut this.file_op_dialog {
                                dialog.error = Some(message.into());
                                cx.notify();
                            }
                        } else {
                            this.push_holt_notice(HoltNoticeKind::Error, message.into(), cx);
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// The renamed entry's absolute destination (same directory, new name) —
    /// mirrors the engine's sibling-rename rule for tab bookkeeping.
    fn rename_destination_hint(&self, new_name: &str, parent: &str) -> String {
        let parent = parent.trim_end_matches('/');
        if parent.is_empty() {
            format!("/{new_name}")
        } else {
            format!("{parent}/{new_name}")
        }
    }

    /// A rename moves tabs for the SAME file and every file under a renamed
    /// directory: paths rewrite, drafts and modified state ride along
    /// untouched, and later saves reach the renamed location. A tab whose
    /// RESOLVED target lives under the moved directory (opened through an
    /// alias) keeps its entry spelling and follows with its resolved
    /// identity only (ticket 07).
    fn carry_tabs_over_rename(
        &mut self,
        space: &str,
        from: &str,
        to: &str,
        cx: &mut Context<Self>,
    ) {
        let tabs: Vec<(u64, String, Option<String>)> = self
            .file_state
            .space(space)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .map(|tab| (tab.id, tab.path.clone(), tab.resolved.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (id, path, resolved) in tabs {
            let Some(new_path) = rewritten_path(from, to, &path) else {
                // The entry spelling didn't move; a resolved target under
                // the moved directory still has to follow.
                if let Some(resolved) = resolved
                    && let Some(new_resolved) = rewritten_path(from, to, &resolved)
                    && let Some(tab) = self
                        .file_state
                        .get(space)
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.id == id)
                {
                    tab.resolved = Some(new_resolved.clone());
                    let viewer = tab.viewer.clone();
                    viewer.update(cx, |viewer, _| viewer.move_resolved_to(new_resolved));
                }
                continue;
            };
            if let Some(tab) = self
                .file_state
                .get(space)
                .tabs
                .iter_mut()
                .find(|tab| tab.id == id)
            {
                tab.path = new_path.clone();
                tab.resolved = None;
                let viewer = tab.viewer.clone();
                viewer.update(cx, |viewer, _| viewer.move_to(new_path));
            }
        }
    }

    /// Direct tree refresh for one directory ("" = the root) — successful
    /// mutations update the visible tree without waiting for the watch.
    fn refresh_tree_dir(&mut self, dir: &str, cx: &mut Context<Self>) {
        if let Some(tree) = &self.file_tree {
            tree.update(cx, |tree, cx| tree.refresh_dir(dir, cx));
        }
    }
}

/// A pending cut (ticket 07): paste moves the entry into a chosen directory
/// within the SAME Space — the binding is what keeps a navigation away from
/// turning a paste into a cross-Space mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileCut {
    pub space: String,
    pub path: String,
    pub name: String,
}

/// A trash request held for its Save-all / Discard-all / Cancel decision
/// (ticket 07). `scope` is the RPC selector bound when the request started —
/// the decision may land after the user navigated, and the trash must still
/// act on the owning Space, never the newly selected one.
pub(super) struct FileTrashConfirm {
    pub space: String,
    pub path: String,
    /// The affected modified tabs whose drafts must resolve before the
    /// entry may leave — including another Chat's shared drafts (ADR-0020),
    /// hidden or not.
    pub dirty: Vec<u64>,
    pub scope: FileScope,
}

/// Does a management operation on `entry` (the entry itself or its whole
/// subtree) touch a tab opened at `path` — or through an alias resolving
/// into the subtree? Pure; unit-tested.
fn entry_affects(tab_path: &str, tab_resolved: Option<&str>, entry: &str) -> bool {
    fn under(path: &str, entry: &str) -> bool {
        path == entry || path.starts_with(&format!("{}/", entry.trim_end_matches('/')))
    }
    under(tab_path, entry) || tab_resolved.is_some_and(|resolved| under(resolved, entry))
}

/// Why a paste of the cut entry into `destination_dir` cannot work, if any.
/// Same-directory no-ops and pasting into the entry's own subtree are
/// refused here for an immediate, actionable message; the engine re-raises
/// them (and everything else) behind its own fence. Pure; unit-tested.
fn paste_invalid_reason(cut_path: &str, destination_dir: &str) -> Option<&'static str> {
    let cut = cut_path.trim_end_matches('/');
    let dir = destination_dir.trim_end_matches('/');
    let parent = cut.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
    if dir == parent {
        return Some("It is already in that directory.");
    }
    if dir == cut || dir.starts_with(&format!("{cut}/")) {
        return Some("An entry can't be pasted into itself or its own descendant.");
    }
    None
}

impl Shell {
    /// The RPC selector for a file mutation bound to `space` (ticket 06's
    /// dialog rule): the selected Chat when one exists (its working
    /// directory IS the Space's root), else the Space itself.
    fn file_op_scope(&self, space: &str, cx: &App) -> FileScope {
        let state = self.state.read(cx);
        state
            .selected_chat_row()
            .map(|chat| FileScope {
                chat_id: Some(chat.id.clone()),
                space_id: None,
            })
            .unwrap_or_else(|| FileScope {
                chat_id: None,
                space_id: Some(space.to_string()),
            })
    }

    /// Attach a tree entry to the current chat (ticket 09): the path joins
    /// the composer's staged references — bound to its live target,
    /// deduplicated, owned by whichever draft is showing (path_refs.rs).
    /// Adding never sends a message or starts a Turn, and nothing is read,
    /// copied, or snapshotted: the reference is just a path.
    pub(super) fn attach_entry_to_chat(&mut self, path: &str, cx: &mut Context<Self>) {
        self.composer.update(cx, |composer, cx| {
            composer.add_paths(vec![std::path::PathBuf::from(path)], cx);
        });
    }

    /// Cut an entry (ticket 07): it becomes the pending paste source for
    /// its Space and its tree row dims until it is pasted, re-cut, or gone.
    pub(super) fn cut_file_entry(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(space) = self.file_space_key(cx) else {
            return;
        };
        let name = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(path)
            .to_string();
        self.file_cut = Some(FileCut {
            space: space.clone(),
            path: path.to_string(),
            name,
        });
        self.sync_cut_marker(cx);
        cx.notify();
    }

    /// Mirror the cut state into the tree's per-Space markers (rendering
    /// only — the shell's state is the authority). One cut at a time:
    /// every marker clears, then the current one lands.
    fn sync_cut_marker(&mut self, cx: &mut Context<Self>) {
        let cut = self.file_cut.clone();
        if let Some(tree) = &self.file_tree {
            tree.update(cx, |tree, _| {
                tree.clear_all_cuts();
                if let Some(cut) = &cut {
                    tree.set_cut(&cut.space, Some(&cut.path));
                }
            });
        }
    }

    /// Paste the pending cut into `destination_dir` (ticket 07): a move
    /// within the active Space's root. The engine revalidates everything;
    /// a failure keeps the cut and the source identities for a deliberate
    /// retry, and a success carries every affected tab, draft, and the
    /// tree's expansion/selection to the new identity.
    pub(super) fn paste_file_into(&mut self, destination_dir: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(space) = self.file_space_key(cx) else {
            return;
        };
        let Some(cut) = self.file_cut.clone() else {
            return;
        };
        // A cut never acts on a different Space after navigation.
        if space != cut.space {
            return;
        }
        let destination_dir = destination_dir.to_string();
        if let Some(reason) = paste_invalid_reason(&cut.path, &destination_dir) {
            self.push_holt_notice(HoltNoticeKind::Error, reason.into(), cx);
            return;
        }
        let scope = self.file_op_scope(&cut.space, cx);
        // FileScope::params emits the selector + path envelope; the move
        // adds the destination directory.
        let mut params = scope.params(&cut.path);
        params["destinationDirectory"] = serde_json::json!(destination_dir);
        cx.spawn(async move |this, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                holt_rpc::methods::MOVE_WORKSPACE_ENTRY,
                params,
                std::time::Duration::from_secs(10),
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                match reply {
                    Ok(value) => {
                        let destination = value
                            .get("path")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                // Only a fallback for older spellings — the
                                // engine's canonical reply wins above.
                                let name = cut
                                    .path
                                    .trim_end_matches('/')
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or("");
                                format!("{}/{}", destination_dir.trim_end_matches('/'), name)
                            });
                        this.carry_tabs_over_rename(&cut.space, &cut.path, &destination, cx);
                        if let Some(tree) = &this.file_tree {
                            tree.update(cx, |tree, _| {
                                tree.carry_tree_over_rename(&cut.path, &destination);
                            });
                        }
                        // Both ends refresh directly — the watch supplements.
                        let old_parent = cut
                            .path
                            .rsplit_once('/')
                            .map(|(parent, _)| parent.to_string())
                            .unwrap_or_default();
                        this.refresh_tree_dir(&old_parent, cx);
                        this.refresh_tree_dir(&destination_dir, cx);
                        // The cut is consumed; its marker goes with it.
                        this.file_cut = None;
                        this.clear_cut_markers(cx);
                        this.persist_file_navigation(cx);
                    }
                    Err(message) => {
                        // The source is untouched — the cut stays for a
                        // deliberate retry (or a paste elsewhere).
                        this.push_holt_notice(HoltNoticeKind::Error, message.into(), cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Every open tab in `space` a management operation on `entry` would
    /// take with it (the entry itself or a descendant, entry path or
    /// resolved alias).
    fn affected_tab_ids(&self, space: &str, entry: &str) -> Vec<u64> {
        self.file_state
            .space(space)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .filter(|tab| entry_affects(&tab.path, tab.resolved.as_deref(), entry))
                    .map(|tab| tab.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Start a trash request (ticket 07). Every affected modified buffer —
    /// including another Chat's shared drafts, hidden or not — resolves
    /// first: clean trees go straight to the trash RPC, dirty ones stop for
    /// the Save-all / Discard-all / Cancel decision.
    pub(super) fn request_trash_entry(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(space) = self.file_space_key(cx) else {
            return;
        };
        let scope = self.file_op_scope(&space, cx);
        let dirty: Vec<u64> = self
            .affected_tab_ids(&space, path)
            .into_iter()
            .filter(|id| {
                self.file_state
                    .space(&space)
                    .and_then(|tabs| tabs.find(*id))
                    .is_some_and(|tab| tab.viewer.read(cx).is_dirty())
            })
            .collect();
        if dirty.is_empty() {
            self.trash_entry_now(&space, path, &scope, cx);
        } else {
            self.file_trash_confirm = Some(FileTrashConfirm {
                space,
                path: path.to_string(),
                dirty,
                scope,
            });
            cx.notify();
        }
    }

    /// Cancel the pending trash decision (ticket 07): nothing is deleted,
    /// every draft keeps its state, and the tree is untouched.
    pub(super) fn cancel_trash_confirm(&mut self, cx: &mut Context<Self>) {
        self.file_trash_confirm = None;
        cx.notify();
    }

    /// The trash-decision dialog's verdict. Save runs every affected draft
    /// through its save first — any failure (or conflict) keeps the entry
    /// exactly where it is; Discard proceeds; Cancel keeps everything.
    pub(super) fn resolve_trash_confirm(&mut self, save: bool, cx: &mut Context<Self>) {
        let Some(pending) = self.file_trash_confirm.take() else {
            return;
        };
        if !save {
            let scope = pending.scope.clone();
            self.trash_entry_now(&pending.space, &pending.path, &scope, cx);
            return;
        }
        let viewers: Vec<Entity<FileViewer>> = pending
            .dirty
            .iter()
            .filter_map(|id| {
                self.file_state
                    .space(&pending.space)
                    .and_then(|tabs| tabs.find(*id))
                    .map(|tab| tab.viewer.clone())
            })
            .collect();
        let (space, path, scope) = (pending.space, pending.path, pending.scope);
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
                    this.trash_entry_now(&space, &path, &scope, cx);
                } else {
                    // A failed (or conflicting) save prevents the deletion.
                    this.push_holt_notice(
                        HoltNoticeKind::Error,
                        "Could not save every modified file — nothing was moved to the Trash."
                            .into(),
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    /// Run the trash RPC for a resolved request. Success closes every
    /// affected tab (drafts were resolved before the RPC fired), refreshes
    /// the parent directory, and drops a cut that pointed into the entry —
    /// a trashed entry is no longer pasteable. Failure reports and leaves
    /// the entry, its tabs, and any cut state exactly as they were.
    fn trash_entry_now(
        &mut self,
        space: &str,
        path: &str,
        scope: &FileScope,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = scope.params(path);
        let space = space.to_string();
        let path = path.to_string();
        cx.spawn(async move |this, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                holt_rpc::methods::TRASH_WORKSPACE_ENTRY,
                params,
                std::time::Duration::from_secs(15),
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                match reply {
                    Ok(_) => {
                        // The entry left the tree for the Trash: every
                        // affected tab closes (drafts were resolved before
                        // the RPC fired; clean tabs die with their file).
                        for id in this.affected_tab_ids(&space, &path) {
                            this.close_file_tab(&space, id, cx);
                        }
                        // A cut entry that just left is no longer pasteable.
                        if this.file_cut.as_ref().is_some_and(|cut| {
                            cut.space == space && entry_affects(&cut.path, None, &path)
                        }) {
                            this.file_cut = None;
                            this.clear_cut_markers(cx);
                        }
                        let parent = path
                            .rsplit_once('/')
                            .map(|(parent, _)| parent.to_string())
                            .unwrap_or_default();
                        this.refresh_tree_dir(&parent, cx);
                        this.persist_file_navigation(cx);
                    }
                    Err(message) => {
                        // The tree and tabs keep their identities; a
                        // deliberate retry stays possible.
                        this.push_holt_notice(HoltNoticeKind::Error, message.into(), cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Clear the cut markers in EVERY visited Space tree (the shell's cut
    /// state is already gone by the time this runs).
    fn clear_cut_markers(&mut self, cx: &mut Context<Self>) {
        if let Some(tree) = &self.file_tree {
            tree.update(cx, |tree, _| tree.clear_all_cuts());
        }
        cx.notify();
    }
}

/// A rename's path rewrite: the entry itself, or a descendant under a
/// renamed ancestor. Pure — unit-tested.
fn rewritten_path(from: &str, to: &str, path: &str) -> Option<String> {
    if path == from {
        return Some(to.to_string());
    }
    let prefix = format!("{}/", from.trim_end_matches('/'));
    path.strip_prefix(&prefix)
        .map(|rest| format!("{}/{}", to.trim_end_matches('/'), rest))
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

        // Ticket 07: trashing an entry with affected modified buffers.
        // Every draft under the entry resolves first — Save all (any
        // failure aborts the deletion), Discard all, or Cancel.
        let trash_confirm = self
            .file_trash_confirm
            .as_ref()
            .map(|pending| (pending.path.clone(), pending.dirty.len()));
        if let Some((path, count)) = trash_confirm {
            let name = path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(path.as_str())
                .to_string();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.cancel_trash_confirm(cx);
                    }
                }))
                .child(popover::dialog_title(
                    &theme,
                    "Move to Trash with unsaved files?",
                ))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!(
                        "{count} open file(s) under \u{201C}{name}\u{201D} have unsaved changes. Save them before the Trash move? Discarding loses them with the move."
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
                            popover::btn_ghost(&theme, "Cancel", "file-trash-cancel")
                                .id("file-trash-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_trash_confirm(cx);
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Discard all")
                                .id("file-trash-discard")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_trash_confirm(false, cx);
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Save all")
                                .id("file-trash-save")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_trash_confirm(true, cx);
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("file-trash-confirm-dialog", viewport, card));
        }

        overlays
    }
}

impl Shell {
    /// The File tree's context menu (tickets 06 + 07): Paste (when a cut is
    /// pending in this Space), New file / New directory, and — on entry
    /// rows — Cut, Rename…, and Move to Trash, positioned where the
    /// right-click landed.
    pub(super) fn render_file_menu_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays = Vec::new();
        if let Some((target, menu_space)) = self.file_menu.get().cloned() {
            let closing = self.file_menu.closing_since();
            let parent_new_file = target.parent.clone();
            let parent_new_dir = target.parent.clone();
            let entry_target = target.rename.clone();
            let mut menu = popover::popover_card(&theme)
                .w(px(196.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if this.file_menu.begin_close() {
                        popover::reap_popup(cx, |shell: &mut Self| &mut shell.file_menu);
                    }
                    cx.notify();
                }))
                .flex()
                .flex_col();

            // Paste (ticket 07): only when the pending cut belongs to THIS
            // Space — a navigation to another Space never offers it. An
            // invalid destination (the entry's own subtree, its current
            // directory) renders the row disabled with the reason.
            let cut = self.file_cut.clone().filter(|cut| cut.space == menu_space);
            if let Some(cut) = cut {
                let destination = target.parent.clone();
                let invalid = paste_invalid_reason(&cut.path, &destination);
                let mut row =
                    popover::menu_row(&theme, false, "file-menu-paste").id("file-menu-paste");
                row = match invalid {
                    Some(reason) => row.opacity(0.45).tooltip(move |_, cx| {
                        cx.new(|_| {
                            crate::image_viewer::ViewerTooltip(
                                format!("Can't paste here: {reason}").into(),
                            )
                        })
                        .into()
                    }),
                    None => row.on_click(cx.listener(move |this, _, _, cx| {
                        this.close_file_menu(cx);
                        this.paste_file_into(&destination, cx);
                    })),
                };
                menu = menu
                    .child(
                        row.child(
                            icon(icons::CLIPBOARD_PASTE)
                                .size(px(15.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(format!("Paste “{}”", cut.name))),
                    )
                    .child(popover::menu_separator());
            }

            menu = menu
                .child(
                    popover::menu_row(&theme, false, "file-menu-new-file")
                        .id("file-menu-new-file")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_file_menu(cx);
                            this.open_file_op_dialog(
                                file_sidebar_kind_new_file(&parent_new_file),
                                cx,
                            );
                        }))
                        .child(
                            icon(icons::DOCUMENT_ADD)
                                .size(px(15.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("New file")),
                )
                .child(
                    popover::menu_row(&theme, false, "file-menu-new-dir")
                        .id("file-menu-new-dir")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_file_menu(cx);
                            this.open_file_op_dialog(
                                file_sidebar_kind_new_dir(&parent_new_dir),
                                cx,
                            );
                        }))
                        .child(
                            icon(icons::FOLDER_WITH_FILES)
                                .size(px(15.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("New directory")),
                );

            // Entry operations: add-to-chat (ticket 09), cut/paste-move,
            // rename, and trash (tickets 06 + 07). The whole section is
            // row-bound — the background menu offers creates and pastes only.
            if let Some((path, _name)) = entry_target {
                // Ticket 09: attach the entry to the current chat's composer
                // as a path reference — never a send, never a Turn.
                if let Some(attach) = target.attach.clone() {
                    menu = menu.child(
                        popover::menu_row(&theme, false, "file-menu-attach")
                            .id("file-menu-attach")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_file_menu(cx);
                                this.attach_entry_to_chat(&attach, cx);
                            }))
                            .child(
                                icon(icons::PAPERCLIP)
                                    .size(px(15.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Add to Chat")),
                    );
                }
                let cut_path = path.clone();
                menu = menu.child(popover::menu_separator()).child(
                    popover::menu_row(&theme, false, "file-menu-cut")
                        .id("file-menu-cut")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_file_menu(cx);
                            this.cut_file_entry(&cut_path, cx);
                        }))
                        .child(
                            icon(icons::SCISSORS_CUT)
                                .size(px(15.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Cut")),
                );
                let rename_path = path.clone();
                let trash_path = path.clone();
                menu = menu.child(
                    popover::menu_row(&theme, false, "file-menu-rename")
                        .id("file-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_file_menu(cx);
                            this.open_file_op_dialog(
                                FileOpKind::Rename {
                                    path: rename_path.clone(),
                                },
                                cx,
                            );
                        }))
                        .child(icon(icons::PEN).size(px(15.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                );
                menu = menu.child(popover::menu_separator()).child(
                    popover::menu_row(&theme, false, "file-menu-trash")
                        .id("file-menu-trash")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_file_menu(cx);
                            this.request_trash_entry(&trash_path, cx);
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(15.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Move to Trash")),
                );
            }
            let menu = menu.into_any_element();
            overlays.push(popover::menu_at(
                "file-context-menu",
                target.position,
                menu,
                closing,
            ));
        }

        if let Some(dialog) = &mut self.file_op_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                let handle = gpui::Focusable::focus_handle(dialog.input.read(cx), cx).clone();
                window.focus(&handle, cx);
            }
            let theme = theme.clone();
            let (title, hint) = match &dialog.kind {
                FileOpKind::NewFile { parent } => ("New file", destination_hint(parent)),
                FileOpKind::NewDirectory { parent } => ("New directory", destination_hint(parent)),
                FileOpKind::Rename { .. } => ("Rename", String::new()),
            };
            let error = dialog.error.clone();
            let pending = dialog.pending;
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.close_file_op_dialog(cx);
                    }
                }))
                .child(popover::dialog_title(&theme, title))
                .when(!hint.is_empty(), |card| {
                    card.child(
                        div().mt(px(4.0)).child(
                            div()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted)
                                .child(hint),
                        ),
                    )
                })
                .child(
                    div()
                        .mt(px(10.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .when_some(error, |card, message| {
                    card.child(
                        div().mt(px(8.0)).child(
                            div()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.danger_muted)
                                .child(message),
                        ),
                    )
                })
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "file-op-cancel")
                                .id("file-op-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_file_op_dialog(cx);
                                })),
                        )
                        .child(
                            popover::btn_primary(
                                &theme,
                                if pending {
                                    "Working…"
                                } else if matches!(dialog.kind, FileOpKind::Rename { .. }) {
                                    "Rename"
                                } else {
                                    "Create"
                                },
                            )
                            .id("file-op-submit")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit_file_op(cx);
                            })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("file-op-dialog", viewport, card));
        }

        overlays
    }

    fn close_file_menu(&mut self, cx: &mut Context<Self>) {
        if self.file_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.file_menu);
        }
        cx.notify();
    }
}

fn file_sidebar_kind_new_file(parent: &str) -> FileOpKind {
    FileOpKind::NewFile {
        parent: parent.to_string(),
    }
}

fn file_sidebar_kind_new_dir(parent: &str) -> FileOpKind {
    FileOpKind::NewDirectory {
        parent: parent.to_string(),
    }
}

/// The muted "lands in …" hint under a create dialog's title.
fn destination_hint(parent: &str) -> String {
    if parent.is_empty() {
        String::new()
    } else {
        format!("In {}", parent)
    }
}

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn renamed_paths_rewrite_for_entries_and_descendants() {
        // The entry itself.
        assert_eq!(
            rewritten_path("/r/old.rs", "/r/new.rs", "/r/old.rs"),
            Some("/r/new.rs".to_string())
        );
        // A descendant of a renamed directory keeps its tail.
        assert_eq!(
            rewritten_path("/r/pkg", "/r/crate", "/r/pkg/src/main.rs"),
            Some("/r/crate/src/main.rs".to_string())
        );
        // Unrelated paths and near-miss prefixes stay untouched.
        assert_eq!(rewritten_path("/r/pkg", "/r/crate", "/r/pkgx/a"), None);
        assert_eq!(rewritten_path("/r/pkg", "/r/crate", "/r/other"), None);
        // Trailing separators normalize.
        assert_eq!(
            rewritten_path("/r/pkg/", "/r/crate", "/r/pkg/deep/x"),
            Some("/r/crate/deep/x".to_string())
        );
    }

    #[test]
    fn entry_names_validate_like_the_engine() {
        assert!(Shell::validate_entry_name("notes draft ✓.md").is_ok());
        assert!(Shell::validate_entry_name("a/b").is_err());
        assert!(Shell::validate_entry_name(".git").is_err());
        assert!(Shell::validate_entry_name("..").is_err());
        assert!(Shell::validate_entry_name("  ").is_err());
    }
}

/// Ticket 11: File as a right-pane surface — the picker choice reveals the
/// tree column and the contents pane, resolves to the Space's live tab, and
/// the Chat-owned surfaces never move the tree column.
#[cfg(test)]
mod surface_tests {
    use super::*;
    use crate::files::FileTab;
    use crate::files::viewer::FileViewer;

    #[gpui::test]
    fn file_surface_choice_reveals_tree_and_contents(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = crate::state::AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let mut shell = Shell::new(
                state,
                crate::state::EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.splash = SplashPhase::Gone;
            shell.route = Route::Chat;
            shell
        });

        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                // The reference launch arrangement shows the tree already;
                // hide it so the reveal is observable.
                assert!(shell.file_tree_visible);
                shell.set_file_tree_visible(false, cx);
                assert!(!shell.file_tree_visible);
                assert!(!shell.right_pane_open(cx));

                // Choosing File reveals BOTH the tree column and the shared
                // contents area, and lands on the File surface.
                assert!(shell.add_file_surface(cx));
                assert!(shell.file_tree_visible);
                assert!(shell.right_pane_open(cx));
                assert_eq!(
                    shell.panels.get(&shell.panel_key(cx)).right_active,
                    RightSurface::Files
                );

                // With no live tab the surface resolves to its own empty
                // state — never a dead tab or the surface picker.
                assert_eq!(shell.resolved_right_active(cx), RightSurface::Files);

                // Choosing File again is not a fresh reveal (no focus steal).
                assert!(!shell.add_file_surface(cx));
            });
        });
    }

    #[gpui::test]
    fn file_surface_shows_the_space_selected_tab(cx: &mut gpui::TestAppContext) {
        // `open_file` persists navigation: give the debounced settings store
        // a real home for this test app.
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let state = cx.new(|_| {
            let mut state = crate::state::AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let mut shell = Shell::new(
                state,
                crate::state::EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.splash = SplashPhase::Gone;
            shell.route = Route::Chat;
            shell
        });

        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                shell.add_file_surface(cx);

                // The tree-open path (`open_file`) stores the TAB as the
                // chat's pick, and closing it heals to the surface picker —
                // the pane's add-a-surface state (no rows left).
                shell.open_file("/tmp/space-1/notes.md".into(), None, true, cx);
                assert_eq!(
                    shell.panels.get(&shell.panel_key(cx)).right_active,
                    RightSurface::File(1)
                );
                assert_eq!(shell.resolved_right_active(cx), RightSurface::File(1));
                shell.close_file_tab("space-1", 1, cx);
                assert_eq!(
                    shell.panels.get(&shell.panel_key(cx)).right_active,
                    RightSurface::Picker
                );
                assert_eq!(shell.resolved_right_active(cx), RightSurface::Picker);

                // The File SURFACE pick (the picker card, chosen with no tabs
                // open) is different: it renders the Space's live selection.
                let viewer = cx.new(|cx| {
                    FileViewer::new(
                        shell.state.clone(),
                        "/tmp/space-1/notes.md".into(),
                        FileScope {
                            chat_id: None,
                            space_id: Some("space-1".into()),
                        },
                        cx,
                    )
                });
                shell.file_state.get("space-1").tabs.push(FileTab {
                    id: 4,
                    path: "/tmp/space-1/notes.md".into(),
                    resolved: None,
                    pinned: true,
                    viewer,
                });
                shell.file_state.set_active("space-1", 4);
                shell.set_right_active(RightSurface::Files, cx);

                // The strip highlights that chip and the surface actions
                // (Cmd+S etc.) address it.
                assert_eq!(shell.resolved_right_active(cx), RightSurface::File(4));

                // Closing it heals to the File surface's own empty state —
                // the pick never points at a dead tab.
                shell.close_file_tab("space-1", 4, cx);
                assert_eq!(
                    shell.panels.get(&shell.panel_key(cx)).right_active,
                    RightSurface::Files
                );
                assert_eq!(shell.resolved_right_active(cx), RightSurface::Files);
            });
        });
    }

    #[gpui::test]
    fn chat_owned_surfaces_leave_the_tree_column_alone(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = crate::state::AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let mut shell = Shell::new(
                state,
                crate::state::EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.splash = SplashPhase::Gone;
            shell.route = Route::Chat;
            shell
        });

        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                shell.set_file_tree_visible(false, cx);

                // A Git surface opens the contents area only — the tree
                // column follows its own visibility, never the surface. (The
                // picker lives inside the pane, so the UI path finds it
                // open; mirror that here.)
                shell.toggle_right_pane(cx);
                shell.add_diff_surface(cx);
                assert!(!shell.file_tree_visible);
                assert!(shell.right_pane_open(cx));
                assert!(matches!(
                    shell.resolved_right_active(cx),
                    RightSurface::Diff(_)
                ));

                // The tree and the Chat-owned surfaces coexist when the user
                // opens the column beside them.
                shell.set_file_tree_visible(true, cx);
                assert!(shell.file_tree_visible);
                assert!(matches!(
                    shell.resolved_right_active(cx),
                    RightSurface::Diff(_)
                ));

                // A Terminal choice (no engine here, so no tab opens) also
                // leaves the column exactly as it was.
                shell.add_terminal_surface(cx);
                assert!(shell.file_tree_visible);
            });
        });
    }
}

#[cfg(test)]
mod cut_paste_trash_tests {
    use super::*;
    use crate::files::FileTab;
    #[test]
    fn entry_affects_matches_entries_descendants_and_alias_resolved_tabs() {
        // The entry itself and a true descendant.
        assert!(entry_affects("/r/pkg", None, "/r/pkg"));
        assert!(entry_affects("/r/pkg/src/main.rs", None, "/r/pkg"));
        // Near-miss prefixes do not match.
        assert!(!entry_affects("/r/pkgx/a", None, "/r/pkg"));
        assert!(!entry_affects("/r/other", None, "/r/pkg"));
        // A tab opened through an alias that resolves INTO the subtree is
        // affected even though its entry path says otherwise.
        assert!(entry_affects(
            "/r/alias.md",
            Some("/r/pkg/real.md"),
            "/r/pkg"
        ));
        // An alias pointing elsewhere is not.
        assert!(!entry_affects("/r/alias.md", Some("/r/other.md"), "/r/pkg"));
        // Trailing separators on the entry normalize.
        assert!(entry_affects("/r/pkg/a", None, "/r/pkg/"));
    }

    #[test]
    fn paste_destinations_validate_like_the_engine() {
        // Pasting into the entry's current directory is a no-op.
        assert_eq!(
            paste_invalid_reason("/r/src/main.rs", "/r/src"),
            Some("It is already in that directory.")
        );
        // Trailing slashes and root-relative shapes normalize the same way.
        assert_eq!(
            paste_invalid_reason("/r/src/main.rs", "/r/src/"),
            Some("It is already in that directory.")
        );
        // A top-level entry pasted into the root's canonical spelling is
        // caught here; the root's "" spelling needs the engine's canonical
        // comparison (it refuses as a same-directory no-op).
        assert_eq!(
            paste_invalid_reason("/r/pkg", "/r"),
            Some("It is already in that directory.")
        );
        assert_eq!(paste_invalid_reason("/r/pkg", ""), None);
        // Into itself and into its own descendant.
        assert!(paste_invalid_reason("/r/pkg", "/r/pkg").is_some());
        assert!(paste_invalid_reason("/r/pkg", "/r/pkg/inner").is_some());
        // Near-miss prefixes are fine; ordinary destinations are fine.
        assert_eq!(paste_invalid_reason("/r/pkg", "/r/pkgx"), None);
        assert_eq!(paste_invalid_reason("/r/pkg", "/r/docs"), None);
        assert_eq!(paste_invalid_reason("/r/src/main.rs", ""), None);
    }

    /// Ticket 07's draft gate at the Shell seam: trashing a directory that
    /// carries a modified buffer (a descendant — another Chat's shared
    /// draft counts the same, ADR-0020) holds for the Save/Discard/Cancel
    /// decision, Cancel deletes nothing, and a clean tree raises no dialog.
    #[gpui::test]
    fn trash_holds_for_dirty_descendants_and_cancel_deletes_nothing(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let mut shell = Shell::new(
                state,
                EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.splash = SplashPhase::Gone;
            shell.route = Route::Chat;
            shell
        });

        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                // One modified tab two levels under the entry being trashed.
                let viewer = cx.new(|cx| {
                    FileViewer::new(
                        shell.state.clone(),
                        "/tmp/space-1/pkg/deep/dirty.rs".into(),
                        FileScope {
                            chat_id: None,
                            space_id: Some("space-1".into()),
                        },
                        cx,
                    )
                });
                viewer.update(cx, |viewer, cx| viewer.mark_dirty_for_test(cx));
                shell.file_state.get("space-1").tabs.push(FileTab {
                    id: 7,
                    path: "/tmp/space-1/pkg/deep/dirty.rs".into(),
                    resolved: None,
                    pinned: true,
                    viewer,
                });

                // Trashing the ancestor directory stops for the draft.
                shell.request_trash_entry("/tmp/space-1/pkg", cx);
                let pending = shell
                    .file_trash_confirm
                    .as_ref()
                    .expect("held for the dirty descendant");
                assert_eq!(pending.dirty, vec![7]);
                assert_eq!(pending.space, "space-1");

                // Cancel prevents the deletion entirely: the decision state
                // clears and the tab (with its draft) stays.
                shell.cancel_trash_confirm(cx);
                assert!(shell.file_trash_confirm.is_none());
                assert_eq!(shell.file_state.space("space-1").unwrap().tabs.len(), 1);

                // An entry with no affected modified buffers never raises
                // the dialog — it goes straight to the trash attempt (no
                // engine in this test, so the attempt itself no-ops).
                shell.request_trash_entry("/tmp/space-1/elsewhere", cx);
                assert!(
                    shell.file_trash_confirm.is_none(),
                    "a clean tree raises no draft decision"
                );
            });
        });
    }
}
