//! The file contents tab: an editable [`CodeEditor`] fed by the engine's
//! `ReadWorkspaceFile`, with explicit Cmd+S saving through
//! `SaveWorkspaceFile`. The viewer owns the draft state — the buffer mirror,
//! the saved baseline, the disk version the draft is based on, and the
//! in-flight save — so a reply applies to the Space/Chat that opened the
//! file, never to whichever chat is selected when it lands. Unsupported
//! content (oversized, non-UTF-8, binary) stays read-only with an
//! external-open action.

use std::ops::Range;

use gpui::prelude::*;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, SharedString, Task, WeakEntity,
    Window, div, px,
};

use super::editor::{CodeEditor, EDITOR_LINE_HEIGHT, EDITOR_TEXT_SIZE, EditorEvent};
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
}

enum ViewerState {
    Loading,
    Editable { editor: Entity<CodeEditor> },
    Unsupported { reason: SharedString },
    Failed { message: SharedString },
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
    /// Bumped per query/content change; a result for an older pair is
    /// dropped, never applied.
    search_generation: u64,
    search_task: Option<Task<()>>,
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
            search_generation: 0,
            search_task: None,
        };
        viewer.load(cx);
        viewer
    }

    /// Open (or close) the in-file search bar, focusing its input.
    pub fn toggle_search(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
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
                        viewer.view = match read.text.clone() {
                            Some(text) => {
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
                            None => ViewerState::Unsupported {
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
                        // Staying conflicting is the honest interim state —
                        // ticket 04 adds reload / Save As / confirmed
                        // overwrite.
                        let _ = conflict;
                        viewer.save_error = Some(
                            "The file changed on disk since it was read. Your changes were kept."
                                .into(),
                        );
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
            .children(search_bar)
            .children(error_banner)
            .child(content)
            .into_any_element()
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
