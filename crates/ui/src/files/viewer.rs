//! The file contents tab: an editable [`CodeEditor`] fed by the engine's
//! `ReadWorkspaceFile`, with explicit Cmd+S saving through
//! `SaveWorkspaceFile`. The viewer owns the draft state — the buffer mirror,
//! the saved baseline, the disk version the draft is based on, and the
//! in-flight save — so a reply applies to the Space/Chat that opened the
//! file, never to whichever chat is selected when it lands. Unsupported
//! content (oversized, non-UTF-8, binary) stays read-only with an
//! external-open action.

use gpui::prelude::*;

use gpui::{AnyElement, Context, Entity, EventEmitter, SharedString, Task, WeakEntity, div, px};

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
        };
        viewer.load(cx);
        viewer
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
                                let editor = cx.new(|cx| {
                                    let mut editor = CodeEditor::new(cx);
                                    editor.load(buffer, cx);
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
            .children(error_banner)
            .child(content)
            .into_any_element()
    }
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
