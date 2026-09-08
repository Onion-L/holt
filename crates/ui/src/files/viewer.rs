//! The file contents tab: a read-only (ticket 01) viewer fed by the engine's
//! `ReadWorkspaceFile`. Text renders as a virtualized monospace line list so
//! a file near the 2 MiB ceiling stays responsive; unsupported content
//! (oversized, non-UTF-8, binary) and failures get distinct, actionable
//! states with an external-open action.

use gpui::prelude::*;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, ListState, SharedString, WeakEntity, div, list, px,
};

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

/// Events up to the shell (ticket 02 adds edit-pinning and dirty markers).
#[allow(dead_code)]
pub enum FileViewerEvent {
    /// The first read answered — the shell records the resolved path for
    /// alias-aware duplicate detection.
    Loaded { resolved: String },
}

enum ViewerState {
    Loading,
    Ready {
        /// Display lines (terminators stripped for rendering).
        lines: Vec<SharedString>,
    },
    Unsupported {
        reason: SharedString,
    },
    Failed {
        message: SharedString,
    },
}

pub struct FileViewer {
    state: Entity<AppState>,
    /// The path this tab opened (the tree entry's own path, maybe a symlink).
    path: String,
    /// The scope the read is bound to (owning Space/Chat at open time).
    scope: FileScope,
    /// The engine-resolved canonical path after the first read.
    resolved: Option<String>,
    view: ViewerState,
    /// The full read reply's source facts (version token, BOM, line
    /// endings) — ticket 02's save baseline.
    facts: Option<holt_proto::WorkspaceFileRead>,
    /// Invalidation token: bumped when a fresh read supersedes an older one.
    generation: u64,
    list: ListState,
}

impl EventEmitter<FileViewerEvent> for FileViewer {}

impl gpui::Render for FileViewer {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The scroller is this pane's own surface (not nested in another
        // scroller), but rows are nowrap: the horizontal scroll lives on the
        // wrapper so long lines never overlap the surrounding chrome.
        div()
            .id("file-viewer")
            .size_full()
            .overflow_x_scroll()
            .child(self.render_body(cx))
    }
}

pub(crate) const LINE_HEIGHT: f32 = 20.0;

impl FileViewer {
    pub fn new(
        state: Entity<AppState>,
        path: String,
        scope: FileScope,
        cx: &mut Context<Self>,
    ) -> Self {
        let list = ListState::new(0, gpui::ListAlignment::Top, px(200.0))
            .with_uniform_item_height(px(LINE_HEIGHT));
        let mut viewer = Self {
            state,
            path,
            scope,
            resolved: None,
            view: ViewerState::Loading,
            facts: None,
            generation: 0,
            list,
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
                                let lines = split_display_lines(&text);
                                viewer.list.reset(lines.len());
                                ViewerState::Ready { lines }
                            }
                            None => ViewerState::Unsupported {
                                reason: read
                                    .unsupported_reason
                                    .clone()
                                    .unwrap_or_else(|| "This file cannot be opened.".into())
                                    .into(),
                            },
                        };
                        viewer.facts = Some(read);
                        cx.emit(FileViewerEvent::Loaded { resolved });
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

    pub(super) fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        match &self.view {
            ViewerState::Loading => centered_muted("Loading…", &theme),
            ViewerState::Unsupported { reason } => {
                unsupported_state(&self.path, reason.clone(), &theme, cx)
            }
            ViewerState::Failed { message } => {
                unsupported_state(&self.path, message.clone(), &theme, cx)
            }
            ViewerState::Ready { .. } => {
                // Plain `list` (not inside another scroller): the pane's own
                // vertical surface; rows are nowrap so long lines widen the
                // pane's horizontal scroller instead of overlapping chrome.
                list(
                    self.list.clone(),
                    cx.processor(move |this, ix, _, cx| this.render_line(ix, cx)),
                )
                .size_full()
                .into_any_element()
            }
        }
    }

    fn render_line(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let ViewerState::Ready { lines } = &self.view else {
            return div().into_any_element();
        };
        let Some(line) = lines.get(ix) else {
            return div().into_any_element();
        };
        div()
            .h(px(LINE_HEIGHT))
            .w_full()
            .px(px(12.0))
            .flex()
            .items_center()
            .whitespace_nowrap()
            .text_size(crate::typography::ui_rems(12.0))
            .line_height(px(LINE_HEIGHT))
            .font_family(theme.font_mono.clone())
            .text_color(theme.text.opacity(0.88))
            .child(line.clone())
            .into_any_element()
    }
}

/// Split into display lines: keep every byte of the text (the save flow
/// reproduces it verbatim); only the terminator hides from the row — a
/// CRLF pair hides whole, a lone `\r` stays visible (conservative).
fn split_display_lines(text: &str) -> Vec<SharedString> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        match character {
            '\n' => {
                if current.ends_with('\r') {
                    current.pop();
                }
                lines.push(std::mem::take(&mut current).into());
            }
            other => current.push(other),
        }
    }
    lines.push(current.into());
    lines
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
mod tests {
    use super::*;

    #[test]
    fn display_lines_split_on_lf_and_hide_crlf_pairs() {
        let lines = split_display_lines("a\nb\r\nc");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].as_ref(), "a");
        assert_eq!(lines[1].as_ref(), "b");
        assert_eq!(lines[2].as_ref(), "c");
    }

    #[test]
    fn empty_text_is_one_line() {
        let lines = split_display_lines("");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].as_ref(), "");
    }

    #[test]
    fn trailing_newline_yields_a_final_empty_line() {
        let lines = split_display_lines("x\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].as_ref(), "");
    }
}
