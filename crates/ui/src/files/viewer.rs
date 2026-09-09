//! The file contents tab: an editable [`CodeEditor`] fed by the engine's
//! `ReadWorkspaceFile`, with explicit Cmd+S saving through
//! `SaveWorkspaceFile`. The viewer owns the draft state — the buffer mirror,
//! the saved baseline, the disk version the draft is based on, and the
//! in-flight save — so a reply applies to the Space/Chat that opened the
//! file, never to whichever chat is selected when it lands. Markdown files
//! additionally switch between source and a rendered preview in the same
//! tab (ticket 08); image files open read-only through the workspace-fenced
//! `ReadWorkspaceImage`; unsupported content (oversized, non-UTF-8, binary)
//! stays read-only with an external-open action.

use std::ops::Range;

use gpui::prelude::*;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, SharedString, Task, WeakEntity,
    Window, div, px,
};

use super::editor::{CodeEditor, EDITOR_LINE_HEIGHT, EDITOR_TEXT_SIZE, EditorEvent};
use super::image_surface::{FileImageSurface, FileImageSurfaceEvent};
use super::preview::{MarkdownPreview, is_markdown_path};
use crate::icons::{self, icon};
use crate::state::AppState;
use crate::theme::Theme;

/// The owning scope a read is bound to — captured when the tab opens so a
/// reply applies to the Space/Chat that requested it, never to whichever
/// Chat happens to be selected when the reply lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileScope {
    pub chat_id: Option<String>,
    pub space_id: Option<String>,
}

impl FileScope {
    pub fn params(&self, path: &str) -> serde_json::Value {
        let mut params = serde_json::Map::new();
        if let Some(chat_id) = &self.chat_id {
            params.insert("chatId".into(), serde_json::json!(chat_id));
        } else if let Some(space_id) = &self.space_id {
            params.insert("spaceId".into(), serde_json::json!(space_id));
        }
        params.insert("path".into(), serde_json::json!(path));
        serde_json::Value::Object(params)
    }
}

/// Which load a reply answers: the first read builds the editor; a reload
/// swaps the buffer in place (cursor and focus survive the refresh).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadMode {
    First,
    Reload,
}

/// Events up to the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileViewerEvent {
    /// The first read answered — the shell records the resolved path for
    /// alias-aware duplicate detection.
    Loaded { resolved: String },
    /// The buffer's modified state flipped (edit, undo back to clean, or a
    /// save acknowledgement).
    DirtyChanged { dirty: bool },
    /// The user edited a preview tab — it must pin immediately (decision 12).
    PinRequested,
    /// Save As landed: the tab now shows `path` (the original file keeps
    /// its disk state).
    Moved { path: String },
}

#[derive(Clone)]
enum ViewerState {
    Loading,
    Editable { editor: Entity<CodeEditor> },
    Image { surface: Entity<FileImageSurface> },
    Unsupported { reason: SharedString },
    Failed { message: SharedString },
}

/// A Markdown tab's mode (ticket 08): the same tab shows either the source
/// editor or the rendered preview of the CURRENT buffer. Switching modes
/// touches no draft state — the editor entity, its undo stack, the dirty
/// flag, and the save baseline all survive round trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MdMode {
    Source,
    Preview,
}

/// The in-flight save: the generation that invalidates a stale
/// acknowledgement, and the watch channel every awaiter (tab close, quit
/// gate, a Cmd+S that joins a save already running) reads the outcome from.
/// The master receiver is multi-consumer — any number of joiners can wait.
struct PendingSave {
    generation: u64,
    done: tokio::sync::watch::Sender<bool>,
    outcome: tokio::sync::watch::Receiver<bool>,
}

pub struct FileViewer {
    state: Entity<AppState>,
    /// The path this tab opened (the tree entry's own path, maybe a symlink).
    path: String,
    /// The scope the read/save is bound to (owning Space/Chat at open time).
    scope: FileScope,
    /// The engine-resolved canonical path after the first read.
    resolved: Option<String>,
    view: ViewerState,
    /// Mirror of the editor's current text, refreshed on every edit event —
    /// lets `&self` methods (strip rendering) read draft state without a
    /// context borrow.
    buffer_text: Option<String>,
    /// The last known-good disk contents — the save baseline.
    saved_text: Option<String>,
    facts: Option<holt_proto::WorkspaceFileRead>,
    pending_save: Option<PendingSave>,
    /// Invalidation token for reads and saves alike.
    generation: u64,
    /// A save error currently shown (kept until the next save attempt).
    save_error: Option<SharedString>,
    /// In-file search (ticket 03): the bar, its query, and the computed
    /// matches. Searching never touches the buffer or its undo history —
    /// the editor only paints what lands in `set_search_matches`.
    search_open: bool,
    search_input: Option<Entity<crate::composer::ComposerInput>>,
    search_matches: Vec<Range<usize>>,
    search_active: Option<usize>,
    /// The conflict state (ticket 04): set when the disk under a DIRTY
    /// buffer changed. `disk_snapshot` is the version the user reviews —
    /// a confirmed overwrite targets exactly it.
    conflicted: bool,
    disk_snapshot: Option<holt_proto::WorkspaceFileRead>,
    /// The compare overlay (draft vs disk) while the conflict is up.
    compare_open: bool,
    /// The Save As destination input (the banner's Save As… opens it).
    save_as_input: Option<Entity<crate::composer::ComposerInput>>,
    /// A restored tab (ticket 05): the first read waits until this viewer
    /// actually renders, so restart never eagerly reads every tab.
    deferred_read: bool,
    /// The reviewed disk token a confirmed overwrite targets (consumed by
    /// the next `save`).
    overwrite_baseline: Option<String>,
    /// Bumped per query/content change; a result for an older pair is
    /// dropped, never applied.
    search_generation: u64,
    search_task: Option<Task<()>>,
    /// The Markdown mode (ticket 08): only meaningful for an editable
    /// Markdown path; every other tab renders source alone.
    md_mode: MdMode,
    /// The preview entity behind `MdMode::Preview` — created on first use,
    /// reused after. Rendering reflects the buffer submitted on entry (and
    /// on clean reloads while the preview is showing).
    preview: Option<Entity<MarkdownPreview>>,
}

impl EventEmitter<FileViewerEvent> for FileViewer {}

impl gpui::Render for FileViewer {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_body(cx)
    }
}

impl FileViewer {
    pub fn new(
        state: Entity<AppState>,
        path: String,
        scope: FileScope,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_mode(state, path, scope, false, cx)
    }

    /// A navigation-restored tab (ticket 05): starts in Loading WITHOUT a
    /// read — the disk loads only when the tab first renders, and no draft
    /// state ever claims recovery.
    pub fn restored(
        state: Entity<AppState>,
        path: String,
        scope: FileScope,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_mode(state, path, scope, true, cx)
    }

    fn with_mode(
        state: Entity<AppState>,
        path: String,
        scope: FileScope,
        deferred: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut viewer = Self {
            state,
            path,
            scope,
            resolved: None,
            view: ViewerState::Loading,
            buffer_text: None,
            saved_text: None,
            facts: None,
            pending_save: None,
            generation: 0,
            save_error: None,
            search_open: false,
            search_input: None,
            search_matches: Vec::new(),
            search_active: None,
            conflicted: false,
            disk_snapshot: None,
            compare_open: false,
            save_as_input: None,
            deferred_read: deferred,
            overwrite_baseline: None,
            search_generation: 0,
            search_task: None,
            md_mode: MdMode::Source,
            preview: None,
        };
        viewer.load(cx);
        viewer
    }

    /// Does a disk-change frame touch this viewer's file (its own path,
    /// resolved target, or an ancestor)?
    pub fn affected_by(&self, paths: &[String]) -> bool {
        let own = self.resolved.clone().unwrap_or_else(|| self.path.clone());
        paths.iter().any(|changed| {
            changed == &own
                || changed == &self.path
                || own.starts_with(&format!("{changed}/"))
                || self.path.starts_with(&format!("{changed}/"))
        })
    }

    /// A watched disk change under this file: clean buffers reload from
    /// disk; dirty buffers enter the conflict state (the draft stays, no
    /// automatic merge or overwrite).
    pub fn on_disk_changed(&mut self, cx: &mut Context<Self>) {
        if self.pending_save.is_some() {
            // The save's own version check decides; a mid-save reload would
            // race the acknowledgement.
            return;
        }
        if self.is_dirty() {
            // Every frame refreshes the reviewed snapshot — a repeated
            // change invalidates the previous decision.
            self.conflicted = true;
            self.refresh_disk_snapshot(cx);
            cx.notify();
        } else {
            self.reload_from_disk(cx);
        }
    }

    /// Re-read the file from disk (clean viewers only — external deletion
    /// or an unreadable target leaves the state alone for the user to see).
    fn reload_from_disk(&mut self, cx: &mut Context<Self>) {
        if self.is_dirty() {
            return;
        }
        self.load_with(cx, LoadMode::Reload);
    }

    /// Fetch the current disk contents as the reviewed conflict baseline.
    fn refresh_disk_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.scope.params(&self.path);
        self.generation += 1;
        let generation = self.generation;
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                holt_rpc::methods::READ_WORKSPACE_FILE,
                params,
                std::time::Duration::from_secs(20),
            )
            .await;
            let _ = this.update(cx, |viewer, cx| {
                if viewer.generation != generation {
                    return;
                }
                if let Ok(value) = reply
                    && let Ok(read) = serde_json::from_value::<holt_proto::WorkspaceFileRead>(value)
                {
                    viewer.disk_snapshot = Some(read);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reload from disk, discarding the draft — explicit, with the loss
    /// stated on the button.
    pub fn conflict_reload(&mut self, cx: &mut Context<Self>) {
        if !self.conflicted {
            return;
        }
        self.conflicted = false;
        self.compare_open = false;
        self.disk_snapshot = None;
        self.save_error = None;
        self.load(cx);
    }

    /// Toggle the draft-vs-disk compare overlay.
    pub fn conflict_toggle_compare(&mut self, cx: &mut Context<Self>) {
        self.compare_open = !self.compare_open;
        if self.compare_open && self.disk_snapshot.is_none() {
            self.refresh_disk_snapshot(cx);
        }
        cx.notify();
    }

    /// Save As: write the draft to a NEW path in the same root. On success
    /// the tab becomes the new file (the original keeps its disk state).
    pub fn conflict_save_as(&mut self, destination: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(buffer) = self.buffer_text.clone() else {
            return;
        };
        let bom = self.facts.as_ref().map(|facts| facts.bom).unwrap_or(false);
        let mut params = serde_json::Map::new();
        if let Some(chat_id) = &self.scope.chat_id {
            params.insert("chatId".into(), serde_json::json!(chat_id));
        } else if let Some(space_id) = &self.scope.space_id {
            params.insert("spaceId".into(), serde_json::json!(space_id));
        }
        params.insert("path".into(), serde_json::json!(destination.clone()));
        params.insert("text".into(), serde_json::json!(buffer));
        params.insert("bom".into(), serde_json::json!(bom));
        let params = serde_json::Value::Object(params);
        self.generation += 1;
        let generation = self.generation;
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                holt_rpc::methods::WRITE_WORKSPACE_FILE_AS,
                params,
                std::time::Duration::from_secs(20),
            )
            .await;
            let _ = this.update(cx, |viewer, cx| {
                if viewer.generation != generation {
                    return;
                }
                let saved: Result<holt_proto::WorkspaceFileSave, String> = match reply {
                    Ok(value) => serde_json::from_value(value).map_err(|error| error.to_string()),
                    Err(message) => Err(message),
                };
                match saved {
                    Ok(saved) if saved.status == holt_proto::WorkspaceSaveStatus::Saved => {
                        // The draft now lives at the destination.
                        viewer.path = destination.clone();
                        viewer.resolved = None;
                        viewer.save_as_input = None;
                        let moved = destination.clone();
                        cx.emit(FileViewerEvent::Moved { path: moved });
                        viewer.saved_text = viewer.buffer_text.clone();
                        viewer.conflicted = false;
                        viewer.compare_open = false;
                        viewer.disk_snapshot = None;
                        viewer.save_error = None;
                        if let (Some(facts), Some(version)) = (viewer.facts.as_mut(), saved.version)
                        {
                            facts.version = version;
                        }
                        cx.emit(FileViewerEvent::DirtyChanged { dirty: false });
                    }
                    Ok(_) => {
                        viewer.save_error = Some("The destination already exists.".into());
                    }
                    Err(message) => {
                        viewer.save_error = Some(message.into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Confirmed overwrite: applies to the disk version the user reviewed
    /// (the snapshot). Another intervening change re-conflicts engine-side.
    pub fn conflict_overwrite(&mut self, cx: &mut Context<Self>) -> Option<Task<bool>> {
        let reviewed = self
            .disk_snapshot
            .as_ref()
            .map(|snapshot| snapshot.version.clone())?;
        // Route through save() with the reviewed token as the baseline.
        self.overwrite_baseline = Some(reviewed);
        let task = self.save(cx);
        self.overwrite_baseline = None;
        if task.is_some() {
            // A successful overwrite clears the conflict on its own when the
            // save settles (the DirtyChanged handler checks clean+conflict).
        }
        task
    }

    /// A rename/move (ticket 06): the tab's identity changes, the draft,
    /// dirty state, and read baseline ride along — the next save writes the
    /// NEW location (rename leaves mtime/size, so the version token still
    /// matches).
    pub fn move_to(&mut self, new_path: String) {
        self.path = new_path;
        self.resolved = None;
        self.conflicted = false;
        self.compare_open = false;
        self.disk_snapshot = None;
    }

    /// A rename/move of a directory the viewer's RESOLVED target lives in
    /// (ticket 07): the entry spelling (possibly an alias) stays, only the
    /// resolved bookkeeping follows the moved subtree.
    pub fn move_resolved_to(&mut self, new_resolved: String) {
        self.resolved = Some(new_resolved);
    }

    /// Test seam: a loaded file with unsaved edits — the editor's typing
    /// path is not reachable from unit tests, and the draft-protection
    /// flows need a dirty viewer.
    #[cfg(test)]
    pub(crate) fn mark_dirty_for_test(&mut self, cx: &mut Context<Self>) {
        self.saved_text = Some("disk\n".into());
        self.buffer_text = Some("disk\nwith edits\n".into());
        cx.emit(FileViewerEvent::DirtyChanged { dirty: true });
        cx.notify();
    }

    /// Test seam: a successfully read editable file — the same editor +
    /// mirror wiring `load_with`'s success path builds, minus the engine.
    #[cfg(test)]
    pub(crate) fn hydrate_editable_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        let buffer = text.to_string();
        let path = self.path.clone();
        let editor = cx.new(|cx| {
            let mut editor = CodeEditor::new(cx);
            editor.load(buffer, cx);
            editor.set_path(path, cx);
            editor
        });
        cx.subscribe(
            &editor,
            |viewer: &mut FileViewer, _, event: &EditorEvent, cx| {
                viewer.on_editor_event(event, cx);
            },
        )
        .detach();
        self.view = ViewerState::Editable { editor };
        self.buffer_text = Some(text.to_string());
        self.saved_text = Some(text.to_string());
        cx.notify();
    }

    /// Test seam: an unsaved edit to the buffer mirror (what an editor
    /// Edited event would leave behind) — the preview's "current buffer"
    /// source, without the typing path.
    #[cfg(test)]
    pub(crate) fn revise_buffer_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer_text = Some(text.to_string());
        cx.emit(FileViewerEvent::DirtyChanged {
            dirty: self.is_dirty(),
        });
        cx.notify();
    }

    /// Test seam: what a successful fenced image read leaves behind — the
    /// pixels on the surface, the resolved identity, the Loaded event.
    #[cfg(test)]
    pub(crate) fn hydrate_image_pixels_for_test(
        &mut self,
        pixels: crate::images::ViewerPixels,
        resolved: String,
        cx: &mut Context<Self>,
    ) {
        let surface = match &self.view {
            ViewerState::Image { surface } => surface.clone(),
            _ => panic!("not an image tab"),
        };
        surface.update(cx, |surface, cx| surface.set_pixels(pixels, cx));
        self.resolved = Some(resolved.clone());
        cx.emit(FileViewerEvent::Loaded { resolved });
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn md_preview_is_active(&self) -> bool {
        self.md_mode == MdMode::Preview
    }

    #[cfg(test)]
    pub(crate) fn preview_entity(&self) -> Option<Entity<MarkdownPreview>> {
        self.preview.clone()
    }

    #[cfg(test)]
    pub(crate) fn image_surface(&self) -> Option<Entity<FileImageSurface>> {
        match &self.view {
            ViewerState::Image { surface } => Some(surface.clone()),
            _ => None,
        }
    }

    /// Test accessors for the deferral contract.
    #[cfg(test)]
    pub(crate) fn is_deferred(&self) -> bool {
        self.deferred_read
    }

    #[cfg(test)]
    pub(crate) fn failure_message(&self) -> Option<String> {
        match &self.view {
            ViewerState::Failed { message } | ViewerState::Unsupported { reason: message } => {
                Some(message.to_string())
            }
            _ => None,
        }
    }

    /// Open (or close) the in-file search bar, focusing its input. Source
    /// mode only — the preview renders, it does not match.
    pub fn toggle_search(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.preview_active() {
            return;
        }
        if self.search_open {
            self.close_search(cx);
            return;
        }
        let input = cx.new(|cx| {
            crate::composer::ComposerInput::with_context("Find in file", "PaletteSearch", cx)
        });
        cx.subscribe(
            &input,
            |viewer: &mut FileViewer, _, event: &crate::composer::ComposerInputEvent, cx| {
                if matches!(event, crate::composer::ComposerInputEvent::Edited) {
                    viewer.on_search_query_changed(cx);
                }
            },
        )
        .detach();
        let handle = gpui::Focusable::focus_handle(input.read(cx), cx).clone();
        window.focus(&handle, cx);
        self.search_input = Some(input);
        self.search_open = true;
        cx.notify();
    }

    pub fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search_open = false;
        self.search_input = None;
        self.search_matches.clear();
        self.search_active = None;
        self.search_generation += 1;
        self.search_task = None;
        if let Some(editor) = self.editor() {
            editor.update(cx, |editor, cx| {
                editor.set_search_matches(Vec::new(), None, cx);
            });
        }
        cx.notify();
    }

    fn on_search_query_changed(&mut self, cx: &mut Context<Self>) {
        self.run_search(cx);
    }

    /// Compute matches off the edit path: the query (case-insensitive
    /// substring) against the current buffer, generation-guarded so a result
    /// for an older query or buffer revision never replaces a newer one.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.search_input.clone() else {
            return;
        };
        let query = input.read(cx).text().to_string();
        let Some(buffer) = self.buffer_text.clone() else {
            return;
        };
        self.search_generation += 1;
        let generation = self.search_generation;
        self.search_task = Some(cx.spawn(async move |this, cx| {
            let matches = cx
                .background_executor()
                .spawn(async move { find_matches(&buffer, &query) })
                .await;
            let _ = this.update(cx, |viewer, cx| {
                if viewer.search_generation != generation {
                    return;
                }
                let active = (!matches.is_empty()).then_some(0usize);
                viewer.search_matches = matches;
                viewer.search_active = active;
                if let Some(editor) = viewer.editor() {
                    let matches = viewer.search_matches.clone();
                    editor.update(cx, |editor, cx| {
                        editor.set_search_matches(matches, active, cx);
                    });
                }
                cx.notify();
            });
        }));
    }

    /// Next/previous match; wraps at both ends.
    pub fn search_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.search_matches.is_empty() {
            return;
        }
        let current = self.search_active.unwrap_or(0);
        let count = self.search_matches.len();
        let next = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.search_active = Some(next);
        if let Some(editor) = self.editor() {
            let matches = self.search_matches.clone();
            let active = self.search_active;
            editor.update(cx, |editor, cx| {
                editor.set_search_matches(matches, active, cx);
            });
        }
        cx.notify();
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn resolved(&self) -> Option<&str> {
        self.resolved.as_deref()
    }

    pub fn editor(&self) -> Option<Entity<CodeEditor>> {
        match &self.view {
            ViewerState::Editable { editor } => Some(editor.clone()),
            _ => None,
        }
    }

    /// The buffer differs from the last known disk contents.
    pub fn is_dirty(&self) -> bool {
        match (&self.buffer_text, &self.saved_text) {
            (Some(buffer), Some(saved)) => buffer != saved,
            _ => false,
        }
    }

    pub fn is_saving(&self) -> bool {
        self.pending_save.is_some()
    }

    /// (Re)read the file from the engine. The request is bound to this
    /// viewer's scope and generation; a stale reply is dropped, never shown.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.load_with(cx, LoadMode::First)
    }

    fn load_with(&mut self, cx: &mut Context<Self>, mode: LoadMode) {
        // Image files never take the text path (ticket 08): their tabs are
        // read-only pixel views fed by the workspace-fenced image read.
        // Every entry point — first open, restore, clean reload — lands here,
        // so the routing holds for all of them.
        if crate::images::is_image_path(&self.path) {
            self.load_image(cx);
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.view = ViewerState::Failed {
                message: "The engine is not available.".into(),
            };
            return;
        };
        self.generation += 1;
        self.view = ViewerState::Loading;
        let params = self.scope.params(&self.path);
        let generation = self.generation;
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                holt_rpc::methods::READ_WORKSPACE_FILE,
                params,
                std::time::Duration::from_secs(20),
            )
            .await;
            let _ = this.update(cx, |viewer, cx| {
                if viewer.generation != generation {
                    return;
                }
                let read: Result<holt_proto::WorkspaceFileRead, String> = match reply {
                    Ok(value) => serde_json::from_value(value).map_err(|error| error.to_string()),
                    Err(message) => Err(message),
                };
                match read {
                    Ok(read) => {
                        let resolved = read.path.clone();
                        viewer.resolved = Some(read.path.clone());
                        viewer.view = match (read.text.clone(), mode) {
                            (Some(text), LoadMode::Reload)
                                if matches!(viewer.view, ViewerState::Editable { .. }) =>
                            {
                                let existing = viewer.editor().expect("checked");
                                existing.update(cx, |editor, cx| {
                                    editor.reload(text.clone(), cx);
                                });
                                viewer.view.clone()
                            }
                            (Some(text), _) => {
                                let buffer = text.clone();
                                let path = viewer.path.clone();
                                let editor = cx.new(|cx| {
                                    let mut editor = CodeEditor::new(cx);
                                    editor.load(buffer, cx);
                                    editor.set_path(path, cx);
                                    editor
                                });
                                cx.subscribe(
                                    &editor,
                                    |viewer: &mut FileViewer, _, event: &EditorEvent, cx| {
                                        viewer.on_editor_event(event, cx);
                                    },
                                )
                                .detach();
                                ViewerState::Editable { editor }
                            }
                            (None, _) => ViewerState::Unsupported {
                                reason: read
                                    .unsupported_reason
                                    .clone()
                                    .unwrap_or_else(|| "This file cannot be opened.".into())
                                    .into(),
                            },
                        };
                        if let Some(text) = read.text.clone() {
                            viewer.buffer_text = Some(text.clone());
                            viewer.saved_text = Some(text);
                        }
                        viewer.facts = Some(read);
                        // A clean reload while the preview is showing feeds
                        // it the fresh buffer — the preview reflects the
                        // tab's current content, saved or not.
                        if viewer.md_mode == MdMode::Preview {
                            viewer.refresh_preview_from_buffer(cx);
                        }
                        cx.emit(FileViewerEvent::Loaded { resolved });
                        cx.emit(FileViewerEvent::DirtyChanged { dirty: false });
                    }
                    Err(message) => {
                        viewer.view = ViewerState::Failed {
                            message: message.into(),
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Load an image tab's pixels through the workspace-fenced read. The
    /// request is bound to this viewer's scope and generation; a stale reply
    /// is dropped before it can reach the surface, and a file that changed
    /// mid-flight re-loads instead of painting a stale decode.
    fn load_image(&mut self, cx: &mut Context<Self>) {
        // The surface exists from the first routed load — engine or not —
        // so the tab kind, its error state, and its retry survive anything.
        // It persists across reloads; its view state resets inside it
        // whenever new pixels arrive.
        let surface = match &self.view {
            ViewerState::Image { surface } => surface.clone(),
            _ => {
                let surface = cx.new(|_| FileImageSurface::new());
                cx.subscribe(
                    &surface,
                    |viewer: &mut FileViewer, _, event: &FileImageSurfaceEvent, cx| {
                        match event {
                            FileImageSurfaceEvent::Retry
                                if matches!(viewer.view, ViewerState::Image { .. }) =>
                            {
                                viewer.load_image(cx);
                            }
                            // A failed image read still has its deliberate
                            // external escape hatch — the same action every
                            // other unsupported file offers.
                            FileImageSurfaceEvent::OpenExternal => {
                                super::tree::open_externally(&viewer.path, cx);
                            }
                            FileImageSurfaceEvent::Retry => {}
                        }
                    },
                )
                .detach();
                self.view = ViewerState::Image {
                    surface: surface.clone(),
                };
                surface
            }
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            surface.update(cx, |surface, cx| {
                surface.set_failed("The engine is not available.".into(), cx);
            });
            cx.notify();
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        surface.update(cx, |surface, cx| surface.begin_load(cx));
        let params = self.scope.params(&self.path);
        let fingerprint_at_request = crate::images::fingerprint(&self.path);
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result =
                crate::images::load_workspace_pixels(&engine, params, cx.background_executor())
                    .await;
            let _ = this.update(cx, |viewer, cx| {
                if viewer.generation != generation {
                    return; // a superseding read/reload owns the surface now
                }
                if crate::images::fingerprint(&viewer.path) != fingerprint_at_request {
                    // Changed (or deleted) mid-flight: never paint stale
                    // pixels — re-read; a missing file fails honestly.
                    viewer.load_image(cx);
                    return;
                }
                match result {
                    Ok((pixels, resolved)) => {
                        viewer.resolved = Some(resolved.clone());
                        surface.update(cx, |surface, cx| surface.set_pixels(pixels, cx));
                        cx.emit(FileViewerEvent::Loaded { resolved });
                    }
                    Err(cause) => {
                        surface.update(cx, |surface, cx| surface.set_failed(cause, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Does this tab offer a rendered preview at all (editable Markdown)?
    pub fn preview_available(&self) -> bool {
        matches!(self.view, ViewerState::Editable { .. }) && is_markdown_path(&self.path)
    }

    /// Is the preview mode showing right now?
    pub fn preview_active(&self) -> bool {
        self.md_mode == MdMode::Preview
    }

    /// Switch to the rendered preview: submit the CURRENT buffer — unsaved
    /// edits included — and never touch the draft doing it.
    pub fn select_preview_mode(&mut self, cx: &mut Context<Self>) {
        if !self.preview_available() || self.md_mode == MdMode::Preview {
            return;
        }
        self.md_mode = MdMode::Preview;
        // The search bar is source-mode chrome; leaving it open over a
        // preview would paint matches into a hidden editor.
        if self.search_open {
            self.close_search(cx);
        }
        self.refresh_preview_from_buffer(cx);
        cx.notify();
    }

    /// Return to source: the editor kept its buffer, cursor, undo stack,
    /// dirty flag, and save baseline the whole time — nothing to restore.
    pub fn select_source_mode(&mut self, cx: &mut Context<Self>) {
        if self.md_mode == MdMode::Source {
            return;
        }
        self.md_mode = MdMode::Source;
        cx.notify();
    }

    /// Toggle (the header control and the Cmd+Shift+P action).
    pub fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        if self.md_mode == MdMode::Preview {
            self.select_source_mode(cx);
        } else {
            self.select_preview_mode(cx);
        }
    }

    /// Point the preview at the current buffer. The preview entity's own
    /// generation guard makes an in-flight parse for an older revision
    /// unappliable.
    fn refresh_preview_from_buffer(&mut self, cx: &mut Context<Self>) {
        let text = self.buffer_text.clone().unwrap_or_default();
        match &self.preview {
            Some(preview) => {
                preview.update(cx, |preview, cx| preview.set_text(text, cx));
            }
            None => {
                let key: SharedString = format!("file-preview-{}", cx.entity_id().as_u64()).into();
                let preview = cx.new(|cx| MarkdownPreview::new(key, cx));
                preview.update(cx, |preview, cx| preview.set_text(text, cx));
                self.preview = Some(preview);
            }
        }
    }

    /// The file surface header's mode control (ticket 08): a segmented
    /// Source | Preview chip pair, offered only for editable Markdown.
    pub fn render_header_controls(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.preview_available() {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let source_active = !self.preview_active();
        Some(
            div()
                .id("file-md-mode")
                .flex_none()
                .flex()
                .items_center()
                .gap(px(2.0))
                .p(px(2.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(theme.border)
                .child(mode_chip(
                    &theme,
                    "file-md-source",
                    "Source",
                    source_active,
                    cx.listener(|this, _, _, cx| this.select_source_mode(cx)),
                ))
                .child(mode_chip(
                    &theme,
                    "file-md-preview-toggle",
                    "Preview",
                    !source_active,
                    cx.listener(|this, _, _, cx| this.select_preview_mode(cx)),
                ))
                .into_any_element(),
        )
    }

    /// Editor events: refresh the buffer mirror, pin previews, mark dirty,
    /// forward saves.
    fn on_editor_event(&mut self, event: &EditorEvent, cx: &mut Context<Self>) {
        match event {
            EditorEvent::Edited => {
                if let Some(editor) = self.editor() {
                    self.buffer_text = Some(editor.read(cx).text().to_string());
                }
                // An edit clears the stale save error and re-dirties.
                self.save_error = None;
                let dirty = self.is_dirty();
                if self.search_open {
                    self.run_search(cx);
                }
                cx.emit(FileViewerEvent::PinRequested);
                cx.emit(FileViewerEvent::DirtyChanged { dirty });
                cx.notify();
            }
        }
    }

    /// Cmd+S (and the close/exit Save buttons). One save in flight at a
    /// time — a call that joins one awaits THAT save's outcome, never a
    /// silent success. The acknowledgement marks only the submitted
    /// version: newer edits keep the tab dirty. `None` only when there is
    /// nothing editable to save at all.
    pub fn save(&mut self, cx: &mut Context<Self>) -> Option<Task<bool>> {
        // Join an in-flight save: awaiting its real outcome is the only
        // honest answer for a close or quit decision.
        if let Some(pending) = &self.pending_save {
            let mut outcome = pending.outcome.clone();
            return Some(cx.spawn(async move |_, _| {
                let _ = outcome.changed().await;
                *outcome.borrow()
            }));
        }
        let facts = self.facts.clone()?;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.save_error = Some("The engine is not available.".into());
            cx.notify();
            return None;
        };
        let submitted = match (&self.view, &self.buffer_text) {
            (ViewerState::Editable { editor }, _) => editor.read(cx).text().to_string(),
            (_, Some(buffer)) => buffer.clone(),
            _ => return None,
        };
        self.generation += 1;
        let generation = self.generation;
        let (done, outcome) = tokio::sync::watch::channel(false);
        self.pending_save = Some(PendingSave {
            generation,
            done,
            outcome,
        });
        self.save_error = None;
        let mut params = serde_json::Map::new();
        if let Some(chat_id) = &self.scope.chat_id {
            params.insert("chatId".into(), serde_json::json!(chat_id));
        } else if let Some(space_id) = &self.scope.space_id {
            params.insert("spaceId".into(), serde_json::json!(space_id));
        }
        params.insert("path".into(), serde_json::json!(self.path));
        params.insert("text".into(), serde_json::json!(submitted));
        params.insert("version".into(), serde_json::json!(facts.version));
        if let Some(baseline) = self.overwrite_baseline.clone() {
            params.insert("expectDiskVersion".into(), serde_json::json!(baseline));
        }
        params.insert("bom".into(), serde_json::json!(facts.bom));
        let params = serde_json::Value::Object(params);
        let mut settled = self
            .pending_save
            .as_ref()
            .expect("just inserted")
            .outcome
            .clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                holt_rpc::methods::SAVE_WORKSPACE_FILE,
                params,
                std::time::Duration::from_secs(20),
            )
            .await;
            let _ = this.update(cx, |viewer, cx| {
                // A superseding save/read replaced the pending slot: this
                // acknowledgement belongs to a request no longer in flight.
                let stale = viewer
                    .pending_save
                    .as_ref()
                    .map(|pending| pending.generation != generation)
                    .unwrap_or(true);
                if stale {
                    return;
                }
                let outcome: Result<holt_proto::WorkspaceFileSave, String> = match reply {
                    Ok(value) => serde_json::from_value(value).map_err(|error| error.to_string()),
                    Err(message) => Err(message),
                };
                let succeeded = match outcome {
                    Ok(save)
                        if save.status == holt_proto::WorkspaceSaveStatus::Saved
                            && save.version.is_some() =>
                    {
                        // Only the SUBMITTED version is saved: the baseline
                        // becomes exactly what was sent, so edits that
                        // arrived mid-flight keep the tab dirty.
                        viewer.saved_text = Some(submitted.clone());
                        let version = save.version.expect("guarded");
                        if let Some(facts) = viewer.facts.as_mut() {
                            facts.version = version;
                        }
                        viewer.save_error = None;
                        true
                    }
                    Ok(conflict) => {
                        // Keep the draft AND the read baseline: adopting the
                        // conflict's disk token would let a second plain
                        // save overwrite a version the buffer never saw.
                        // The conflict banner owns the resolution from here.
                        let _ = conflict;
                        viewer.conflicted = true;
                        viewer.save_error = None;
                        viewer.refresh_disk_snapshot(cx);
                        false
                    }
                    Err(message) => {
                        viewer.save_error = Some(message.into());
                        false
                    }
                };
                if let Some(pending) = viewer.pending_save.take() {
                    let _ = pending.done.send(succeeded);
                }
                let dirty = viewer.is_dirty();
                if !dirty {
                    // A settled save that leaves the buffer clean ends any
                    // conflict state (the overwrite landed).
                    viewer.conflicted = false;
                    viewer.compare_open = false;
                    viewer.disk_snapshot = None;
                }
                cx.emit(FileViewerEvent::DirtyChanged { dirty });
                cx.notify();
            });
        })
        .detach();
        Some(cx.spawn(async move |_, _| {
            let _ = settled.changed().await;
            *settled.borrow()
        }))
    }

    pub(super) fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // A restored tab reads the disk the first time it is shown —
        // "as needed", never eagerly at startup.
        if self.deferred_read {
            self.deferred_read = false;
            self.load(cx);
        }
        // A sticky save error rides above the editor (conflict, missing
        // file, io failure — the draft is untouched beneath it).
        let error_banner = self.save_error.clone().map(|message| {
            div()
                .id("file-save-error")
                .flex_none()
                .px(px(10.0))
                .py(px(6.0))
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.danger.opacity(0.08))
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    icon(icons::DANGER_TRIANGLE)
                        .size(px(12.0))
                        .text_color(theme.danger_muted),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.danger_muted)
                        .child(message),
                )
        });
        // The conflict banner (ticket 04): the four resolutions. Reload
        // states its loss; Overwrite targets the reviewed disk version.
        let conflict_banner = self.conflicted.then(|| {
            let theme = theme.clone();
            let save_as_input = self.save_as_input.clone();
            let mut bar = div()
                .id("file-conflict-banner")
                .flex_none()
                .px(px(8.0))
                .py(px(6.0))
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.warning.opacity(0.08))
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    icon(icons::DANGER_TRIANGLE)
                        .size(px(12.0))
                        .text_color(theme.warning_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.warning_muted)
                        .child("This file changed on disk. Your changes are kept."),
                );
            if let Some(input) = save_as_input {
                bar = bar
                    .child(
                        div()
                            .id("conflict-save-as-input")
                            .w(px(240.0))
                            .h(px(22.0))
                            .child(input),
                    )
                    .child(conflict_action(
                        &theme,
                        "conflict-save-as-go",
                        "Save",
                        cx.listener(|this, _, _, cx| {
                            let destination = this
                                .save_as_input
                                .as_ref()
                                .map(|input| input.read(cx).text().to_string())
                                .unwrap_or_default();
                            if !destination.trim().is_empty() {
                                this.save_as_input = None;
                                this.conflict_save_as(destination, cx);
                            }
                        }),
                    ));
            }
            bar = bar
                .child(conflict_action(
                    &theme,
                    "conflict-compare",
                    if self.compare_open {
                        "Hide changes"
                    } else {
                        "Compare"
                    },
                    cx.listener(|this, _, _, cx| this.conflict_toggle_compare(cx)),
                ))
                .child(conflict_action(
                    &theme,
                    "conflict-save-as",
                    "Save As…",
                    cx.listener(|this, _, window, cx| {
                        // Seed a real input with a sibling destination — the
                        // engine resolves relatives against the root and
                        // refuses existing names.
                        let sibling =
                            format!("{}/{}-copy", this.parent_dir(), this.path_basename());
                        let input = cx.new(|cx| {
                            crate::composer::ComposerInput::with_context(
                                "Destination path",
                                "PaletteSearch",
                                cx,
                            )
                        });
                        input.update(cx, |input, cx| input.set_text(sibling, cx));
                        let handle = gpui::Focusable::focus_handle(input.read(cx), cx).clone();
                        window.focus(&handle, cx);
                        this.save_as_input = Some(input);
                        cx.notify();
                    }),
                ))
                .child(conflict_action(
                    &theme,
                    "conflict-reload",
                    "Reload (discard draft)",
                    cx.listener(|this, _, _, cx| {
                        this.conflict_reload(cx);
                    }),
                ))
                .child(conflict_action(
                    &theme,
                    "conflict-overwrite",
                    "Overwrite disk",
                    cx.listener(|this, _, _, cx| {
                        this.conflict_overwrite(cx);
                    }),
                ));
            bar
        });

        // The compare overlay: the draft against the reviewed disk version.
        let compare_overlay = (self.conflicted && self.compare_open).then(|| {
            let rows = self.compare_rows();
            let count = rows.len();
            div()
                .id("file-compare")
                .flex_none()
                .max_h(px(240.0))
                .overflow_y_scroll()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .font_family(theme.font_mono.clone())
                .text_size(px(11.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .children(rows.into_iter().take(400).map(|row| {
                            let (marker, text) = row;
                            let color = match marker {
                                '-' => theme.danger_muted,
                                '+' => theme.success_muted,
                                _ => theme.text_muted.opacity(0.7),
                            };
                            div()
                                .flex()
                                .px(px(8.0))
                                .h(px(16.0))
                                .items_center()
                                .gap(px(6.0))
                                .when(marker == '-', |el| el.bg(theme.danger.opacity(0.10)))
                                .when(marker == '+', |el| el.bg(theme.success.opacity(0.10)))
                                .child(
                                    div()
                                        .w(px(10.0))
                                        .text_color(color)
                                        .child(marker.to_string()),
                                )
                                .child(div().truncate().text_color(color).child(text))
                        }))
                        .when(count > 400, |el| {
                            el.child(
                                div()
                                    .px(px(8.0))
                                    .py(px(2.0))
                                    .text_color(theme.text_muted)
                                    .child(format!("… {} more changed lines", count - 400)),
                            )
                        }),
                )
        });

        // The in-file search bar (ticket 03): query, live count, prev/next.
        let search_bar = self.search_open.then(|| {
            let count = self.search_matches.len();
            let label: SharedString = if self.search_query_is_empty(cx) {
                "".into()
            } else if count == 0 {
                "No matches".into()
            } else {
                format!("{}/{}", self.search_active.map_or(1, |ix| ix + 1), count).into()
            };
            let input = self
                .search_input
                .clone()
                .map(|input| input.into_any_element())
                .unwrap_or_else(|| div().into_any_element());
            div()
                .id("file-search-bar")
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" => this.close_search(cx),
                        "enter" => {
                            cx.stop_propagation();
                            this.search_step(!event.keystroke.modifiers.shift, cx);
                        }
                        _ => {}
                    }
                }))
                .flex_none()
                .h(px(32.0))
                .px(px(8.0))
                .border_b_1()
                .border_color(theme.border)
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    icon(icons::MAGNIFER)
                        .size(px(12.0))
                        .text_color(theme.text_muted),
                )
                .child(div().flex_1().min_w_0().h_full().child(input))
                .child(
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(if label.as_ref() == "No matches" {
                            theme.warning_muted
                        } else {
                            theme.text_muted
                        })
                        .child(label),
                )
                .child(search_icon_button(
                    &theme,
                    "search-prev",
                    icons::ARROW_UP,
                    cx.listener(|this, _, _, cx| this.search_step(false, cx)),
                ))
                .child(search_icon_button(
                    &theme,
                    "search-next",
                    icons::ALT_ARROW_DOWN,
                    cx.listener(|this, _, _, cx| this.search_step(true, cx)),
                ))
                .child(search_icon_button(
                    &theme,
                    "search-close",
                    icons::CLOSE,
                    cx.listener(|this, _, _, cx| this.close_search(cx)),
                ))
        });
        let content: AnyElement = match &self.view {
            ViewerState::Loading => centered_muted("Loading…", &theme),
            ViewerState::Unsupported { reason } => {
                unsupported_state(&self.path, reason.clone(), &theme, cx)
            }
            ViewerState::Failed { message } => {
                unsupported_state(&self.path, message.clone(), &theme, cx)
            }
            ViewerState::Image { surface } => div()
                .id("file-image-host")
                .size_full()
                .flex_1()
                .min_h_0()
                .child(surface.clone())
                .into_any_element(),
            ViewerState::Editable { editor } if self.md_mode == MdMode::Preview => div()
                .id("file-preview-host")
                .size_full()
                .flex_1()
                .min_h_0()
                .children(self.preview.clone())
                .into_any_element(),
            ViewerState::Editable { editor } => div()
                .id("file-editor-surface")
                .size_full()
                .flex_1()
                .min_h_0()
                .font_family(theme.font_mono.clone())
                .text_size(px(EDITOR_TEXT_SIZE))
                .line_height(px(EDITOR_LINE_HEIGHT))
                .text_color(theme.text.opacity(0.9))
                .child(editor.clone())
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(conflict_banner)
            .children(compare_overlay)
            .children(search_bar)
            .children(error_banner)
            .child(content)
            .into_any_element()
    }

    fn parent_dir(&self) -> String {
        std::path::Path::new(&self.path)
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_default()
    }

    fn path_basename(&self) -> String {
        self.path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("file")
            .to_string()
    }

    /// Draft-vs-disk compare rows: `- old`, `+ new`, `· unchanged context`
    /// around each changed region (common prefix/suffix trimming).
    fn compare_rows(&self) -> Vec<(char, SharedString)> {
        let Some(snapshot) = &self.disk_snapshot else {
            return Vec::new();
        };
        let draft: Vec<&str> = self
            .buffer_text
            .as_deref()
            .unwrap_or_default()
            .lines()
            .collect();
        let disk: Vec<&str> = snapshot.text.as_deref().unwrap_or("").lines().collect();
        // Colors resolve at paint time; the rows carry only markers here.
        simple_diff_rows(&disk, &draft)
    }

    fn search_query_is_empty(&self, cx: &App) -> bool {
        self.search_input
            .as_ref()
            .is_some_and(|input| input.read(cx).is_empty())
    }
}

/// Case-insensitive substring matches as BYTE ranges over the original
/// buffer. Folding happens per character with positions tracked, because
/// `to_lowercase` changes byte lengths (U+0130 folds to two chars, the
/// Kelvin sign to one) and folded-buffer offsets would not map back. The
/// query's own newlines are ignored — a match never spans lines.
fn find_matches(buffer: &str, query: &str) -> Vec<Range<usize>> {
    let mut needle: Vec<char> = query
        .chars()
        .filter(|c| *c != '\n' && *c != '\r')
        .flat_map(char::to_lowercase)
        .collect();
    needle.shrink_to_fit();
    if needle.is_empty() {
        return Vec::new();
    }
    // Folded haystack with, per folded char, the byte offset of its source
    // char and that char's byte length.
    let mut hay: Vec<char> = Vec::with_capacity(buffer.len());
    let mut starts: Vec<usize> = Vec::with_capacity(buffer.len());
    let mut lens: Vec<usize> = Vec::with_capacity(buffer.len());
    for (offset, character) in buffer.char_indices() {
        let len = character.len_utf8();
        for folded in character.to_lowercase() {
            hay.push(folded);
            starts.push(offset);
            lens.push(len);
        }
    }
    let mut matches = Vec::new();
    let mut at = 0usize;
    while at + needle.len() <= hay.len() {
        if hay[at..at + needle.len()] == needle[..] {
            let start = starts[at];
            let end = starts[at + needle.len() - 1] + lens[at + needle.len() - 1];
            matches.push(start..end);
            at += needle.len();
        } else {
            at += 1;
        }
    }
    matches
}

/// A conflict action chip.
fn conflict_action(
    theme: &Theme,
    id: &'static str,
    label: &str,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
    div()
        .id(id)
        .flex_none()
        .h(px(20.0))
        .px(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(theme.border)
        .flex()
        .items_center()
        .cursor_pointer()
        .hover(|state| state.bg(crate::theme::wash(0.06)))
        .on_click(handler)
        .child(
            div()
                .text_size(crate::typography::ui_rems(10.5))
                .text_color(theme.text)
                .child(label),
        )
}

/// One half of the Source | Preview segmented control (ticket 08): the
/// active half reads as selected (washed fill, brighter text); both halves
/// stay clickable so the mode is one click away in either direction.
fn mode_chip(
    theme: &Theme,
    id: &'static str,
    label: &'static str,
    active: bool,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(22.0))
        .px(px(8.0))
        .rounded(px(5.0))
        .flex()
        .items_center()
        .cursor_pointer()
        .when(active, |el| el.bg(crate::theme::wash(0.10)))
        .hover(|state| state.bg(crate::theme::wash(0.06)))
        .on_click(handler)
        .child(
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(if active { theme.text } else { theme.text_muted })
                .child(label),
        )
}

/// Minimal honest compare: common prefix/suffix lines are context; the
/// middle block lists every disk line as removed and every draft line as
/// added. No merge, no fine-grained hunking — a review, not an edit.
fn simple_diff_rows(disk: &[&str], draft: &[&str]) -> Vec<(char, SharedString)> {
    let mut prefix = 0usize;
    while prefix < disk.len() && prefix < draft.len() && disk[prefix] == draft[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < disk.len().saturating_sub(prefix)
        && suffix < draft.len().saturating_sub(prefix)
        && disk[disk.len() - 1 - suffix] == draft[draft.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut rows = Vec::new();
    for line in disk.iter().take(prefix) {
        rows.push(('·', (*line).into()));
    }
    for line in &disk[prefix..disk.len() - suffix] {
        rows.push(('-', (*line).into()));
    }
    for line in &draft[prefix..draft.len() - suffix] {
        rows.push(('+', (*line).into()));
    }
    for line in disk.iter().skip(disk.len() - suffix) {
        rows.push(('·', (*line).into()));
    }
    rows
}

/// A small square icon button for the search bar's prev/next/close cluster.
fn search_icon_button(
    theme: &Theme,
    id: &'static str,
    path: &'static str,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(20.0))
        .rounded(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|state| state.bg(crate::theme::wash(0.08)))
        .on_click(handler)
        .child(icon(path).size(px(11.0)).text_color(theme.text_muted))
}

fn centered_muted(label: &str, theme: &Theme) -> AnyElement {
    let label = label.to_string();
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(label),
        )
        .into_any_element()
}

/// Unsupported/failed files: the reason plus an explicit external-open
/// action — never a fake viewer or a lossy decode.
fn unsupported_state(
    path: &str,
    reason: SharedString,
    theme: &Theme,
    cx: &mut Context<FileViewer>,
) -> AnyElement {
    let path = path.to_string();
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(16.0))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.0))
                .max_w(px(360.0))
                .child(
                    icon(icons::DOCUMENT)
                        .size(px(20.0))
                        .text_color(theme.text_muted.opacity(0.6)),
                )
                .child(
                    div()
                        .text_center()
                        .text_size(crate::typography::ui_rems(12.5))
                        .text_color(theme.text_muted)
                        .child(reason),
                )
                .child(
                    div()
                        .id("file-open-external")
                        .h(px(28.0))
                        .px(px(10.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border)
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .hover(|state| state.bg(crate::theme::wash(0.06)))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            super::tree::open_externally(&path, cx);
                        }))
                        .child(
                            icon(icons::ARROW_UP_RIGHT)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text)
                                .child("Open externally"),
                        ),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn diff_rows_mark_the_changed_block() {
        let disk = vec!["one", "two", "three", "four"];
        let draft = vec!["one", "TWO", "three", "four", "five"];
        let rows = simple_diff_rows(&disk, &draft);
        let markers: String = rows.iter().map(|(m, _)| *m).collect();
        assert_eq!(markers, "·---++++");
        assert!(rows.iter().any(|(_, t)| t.as_ref() == "TWO"));
        assert!(rows.iter().any(|(_, t)| t.as_ref() == "five"));
    }

    #[test]
    fn identical_documents_have_no_changed_block() {
        let same = vec!["a", "b"];
        let rows = simple_diff_rows(&same, &same);
        assert!(rows.iter().all(|(m, _)| *m == '·'));
    }
}

#[cfg(test)]
mod restore_tests {
    use super::*;
    use crate::state::AppState;
    use gpui::TestAppContext;

    fn scope() -> FileScope {
        FileScope {
            chat_id: None,
            space_id: Some("space-1".into()),
        }
    }

    fn read_viewer<T>(
        cx: &TestAppContext,
        viewer: &Entity<FileViewer>,
        f: impl Fn(&FileViewer) -> T,
    ) -> T {
        cx.read(|cx| f(viewer.read(cx)))
    }

    #[gpui::test]
    fn restored_viewers_defer_their_first_read_until_render(cx: &mut TestAppContext) {
        // No engine attached: an EAGER viewer would settle Failed right
        // away; a restored one must stay quiet (nothing read) until its
        // tab first renders.
        cx.update(|cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let viewer = cx.new(|cx| FileViewer::restored(state, "/tmp/notes.md".into(), scope(), cx));
        cx.run_until_parked();
        assert!(
            read_viewer(cx, &viewer, |v| v.is_deferred()),
            "restore must not read the file"
        );

        // First render consumes the deferral and starts the (here failing)
        // read — proving reads happen only when shown.
        viewer.update(cx, |viewer, cx| {
            drop(viewer.render_body(cx));
        });
        cx.run_until_parked();
        assert!(!read_viewer(cx, &viewer, |v| v.is_deferred()));
        assert!(
            read_viewer(cx, &viewer, |v| v.failure_message()).is_some(),
            "the deferred read ran (no engine: it failed)"
        );
        // And no draft was fabricated anywhere in between.
        assert!(!read_viewer(cx, &viewer, |v| v.is_dirty()));
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn matches_are_case_insensitive_and_never_span_lines() {
        let matches = find_matches("Ab ra cadabra\nabra", "ABRA");
        assert_eq!(matches, vec![9..13, 14..18]);
        // A query's newlines are ignored, so no match can cross a line.
        assert!(find_matches("a\nb", "a\nb").is_empty());
        assert!(find_matches("anything", "").is_empty());
    }
}

#[cfg(test)]
mod preview_mode_tests {
    use super::*;
    use gpui::TestAppContext;

    fn scope() -> FileScope {
        FileScope {
            chat_id: None,
            space_id: Some("space-1".into()),
        }
    }

    /// The ticket 08 contract: switching modes renders the CURRENT buffer
    /// (unsaved edits included) without saving, discarding, or replacing the
    /// draft — the editor, its undo history, the dirty flag, and both
    /// baselines survive the round trip untouched.
    #[gpui::test]
    fn mode_switches_render_the_draft_and_preserve_everything(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let viewer =
            cx.new(|cx| FileViewer::new(state, "/tmp/space-1/notes.md".into(), scope(), cx));
        cx.run_until_parked();
        // No engine: hydrate the loaded state directly (the read path's
        // editor + mirrors, minus the RPC).
        viewer.update(cx, |viewer, cx| {
            viewer.hydrate_editable_for_test("# Title\n\ndisk body\n", cx);
        });
        let editor_before = viewer.read_with(cx, |viewer, _| viewer.editor());
        let undo_before = viewer.read_with(cx, |viewer, _| {
            viewer
                .editor()
                .map(|editor| editor.read_with(cx, |editor, _| editor.undo_stack_len()))
        });

        // An unsaved edit lands in the buffer mirror.
        viewer.update(cx, |viewer, cx| {
            viewer.revise_buffer_for_test("# Title\n\ndisk body WITH EDITS\n", cx);
        });
        assert!(viewer.read_with(cx, |viewer, _| viewer.is_dirty()));

        // DirtyChanged must NOT fire for the mode switches themselves — only
        // edits and saves move that flag.
        let emissions = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let counted = emissions.clone();
        cx.update(|cx| {
            cx.subscribe(&viewer, move |_, event: &FileViewerEvent, _| {
                if matches!(event, FileViewerEvent::DirtyChanged { .. }) {
                    counted.set(counted.get() + 1);
                }
            })
            .detach();
        });

        // Preview reflects the edited buffer.
        viewer.update(cx, |viewer, cx| viewer.select_preview_mode(cx));
        cx.run_until_parked();
        assert!(viewer.read_with(cx, |viewer, _| viewer.md_preview_is_active()));
        let preview = viewer
            .read_with(cx, |viewer, _| viewer.preview_entity())
            .expect("preview entity");
        preview.read_with(cx, |preview, _| {
            assert_eq!(
                preview.first_paragraph().as_deref(),
                Some("disk body WITH EDITS"),
                "the preview renders the unsaved draft"
            );
        });

        // Back to source: everything the editor owned is intact.
        viewer.update(cx, |viewer, cx| viewer.select_source_mode(cx));
        cx.run_until_parked();
        assert!(!viewer.read_with(cx, |viewer, _| viewer.md_preview_is_active()));
        assert!(viewer.read_with(cx, |viewer, _| viewer.is_dirty()));
        let editor_after = viewer.read_with(cx, |viewer, _| viewer.editor());
        assert_eq!(
            editor_before.map(|e| e.entity_id()),
            editor_after.map(|e| e.entity_id()),
            "the source editor entity survives the round trip"
        );
        let undo_after = viewer.read_with(cx, |viewer, _| {
            viewer
                .editor()
                .map(|editor| editor.read_with(cx, |editor, _| editor.undo_stack_len()))
        });
        assert_eq!(undo_before, undo_after, "undo/redo history is untouched");
        viewer.read_with(cx, |viewer, _| {
            assert_eq!(
                viewer.buffer_text.as_deref(),
                Some("# Title\n\ndisk body WITH EDITS\n")
            );
            assert_eq!(viewer.saved_text.as_deref(), Some("# Title\n\ndisk body\n"));
        });
        assert_eq!(emissions.get(), 0, "mode switches never emit DirtyChanged");
    }

    /// Non-Markdown tabs never offer the preview mode, and a Markdown tab
    /// that has not loaded offers nothing either.
    #[gpui::test]
    fn only_loaded_markdown_tabs_offer_preview(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let rust =
            cx.new(|cx| FileViewer::new(state.clone(), "/tmp/space-1/main.rs".into(), scope(), cx));
        let md = cx.new(|cx| FileViewer::new(state, "/tmp/space-1/notes.md".into(), scope(), cx));
        cx.run_until_parked();

        assert!(rust.read_with(cx, |viewer, _| !viewer.preview_available()));
        // Not loaded (no engine): a Markdown path offers nothing yet.
        assert!(md.read_with(cx, |viewer, _| !viewer.preview_available()));
        md.update(cx, |viewer, cx| {
            viewer.hydrate_editable_for_test("# hi\n", cx);
        });
        assert!(md.read_with(cx, |viewer, _| viewer.preview_available()));
        // The toggle is a no-op without availability.
        rust.update(cx, |viewer, cx| viewer.toggle_preview(cx));
        assert!(rust.read_with(cx, |viewer, _| !viewer.md_preview_is_active()));
    }

    /// A clean reload while the preview shows feeds it the fresh buffer —
    /// the preview is a live view of the tab's content, not a snapshot of
    /// the first entry into preview mode.
    #[gpui::test]
    fn reload_refreshes_an_active_preview(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let viewer =
            cx.new(|cx| FileViewer::new(state, "/tmp/space-1/notes.md".into(), scope(), cx));
        viewer.update(cx, |viewer, cx| {
            viewer.hydrate_editable_for_test("first body\n", cx);
            viewer.select_preview_mode(cx);
        });
        cx.run_until_parked();
        // The reload path's preview re-kick (clean viewers only).
        viewer.update(cx, |viewer, cx| {
            viewer.buffer_text = Some("reloaded body\n".into());
            viewer.saved_text = Some("reloaded body\n".into());
            viewer.refresh_preview_from_buffer(cx);
        });
        cx.run_until_parked();
        let preview = viewer
            .read_with(cx, |viewer, _| viewer.preview_entity())
            .unwrap();
        preview.read_with(cx, |preview, _| {
            assert_eq!(preview.first_paragraph().as_deref(), Some("reloaded body"));
        });
    }
}

#[cfg(test)]
mod image_tab_tests {
    use super::*;
    use gpui::TestAppContext;

    fn pixels() -> crate::images::ViewerPixels {
        crate::images::ViewerPixels {
            pixels: std::sync::Arc::new(gpui::RenderImage::new(smallvec::smallvec![
                image::Frame::new(image::RgbaImage::new(3, 2))
            ])),
            width: 3,
            height: 2,
        }
    }

    fn scope() -> FileScope {
        FileScope {
            chat_id: None,
            space_id: Some("space-1".into()),
        }
    }

    /// Image files route to the read-only pixel surface on EVERY load path —
    /// never to a text editor, never to the binary unsupported state — and
    /// the tab is never a draft. (The fence itself is the engine's contract,
    /// covered in crates/engine/tests/workspace_files_rpc.rs.)
    #[gpui::test]
    fn image_files_route_to_the_pixel_surface(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let viewer =
            cx.new(|cx| FileViewer::new(state, "/tmp/space-1/shot.png".into(), scope(), cx));
        cx.run_until_parked();
        let surface = viewer
            .read_with(cx, |viewer, _| viewer.image_surface())
            .expect("image tabs route to the pixel surface");
        // No engine attached: the surface carries the failure (and its
        // retry), not a fake viewer.
        surface.read_with(cx, |surface, _| {
            let cause = surface.failure_cause().expect("honest failure state");
            assert!(cause.contains("engine"), "{cause}");
        });
        viewer.read_with(cx, |viewer, _| {
            assert!(
                viewer.editor().is_none(),
                "no text editor is built for an image"
            );
            assert!(!viewer.is_dirty(), "an image tab is never a draft");
        });
    }

    /// What a successful fenced read leaves behind: decoded pixels on the
    /// surface and the engine-resolved identity for alias-aware duplicate
    /// opens. A stale decode can never land — the load's generation guard
    /// drops superseded replies before this seam's real counterpart runs.
    #[gpui::test]
    fn landed_pixels_carry_the_resolved_identity(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let viewer =
            cx.new(|cx| FileViewer::new(state, "/tmp/space-1/shot.png".into(), scope(), cx));
        cx.run_until_parked();
        viewer.update(cx, |viewer, cx| {
            viewer.hydrate_image_pixels_for_test(pixels(), "/tmp/space-1/real/shot.png".into(), cx);
        });
        let surface = viewer
            .read_with(cx, |viewer, _| viewer.image_surface())
            .unwrap();
        surface.read_with(cx, |surface, _| {
            assert_eq!(surface.natural_size(), Some((3, 2)));
        });
        viewer.read_with(cx, |viewer, _| {
            assert_eq!(
                viewer.resolved.as_deref(),
                Some("/tmp/space-1/real/shot.png")
            );
        });
    }
}
