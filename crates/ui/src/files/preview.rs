//! The file tab's rendered Markdown preview (ticket 08): the SAME rendering
//! foundation the Transcript uses (`markdown::parser` → `markdown::render`),
//! pointed at the tab's current buffer — unsaved edits included. The source
//! editor stays alive behind the preview; switching modes neither saves,
//! discards, nor replaces the draft.
//!
//! Safety of displayed content: this is the transcript's display-only
//! renderer — styled text runs, no HTML execution, no script evaluation.
//! Embedded image references flatten to link runs (the parser's `Image` tag
//! is a link), so preview rendering never resolves or reads a local
//! resource; when an image IS read for a file tab, it goes through the
//! workspace-fenced `ReadWorkspaceImage`, never the general image RPC.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{AnyElement, Context, Render, SharedString, Task, Window, div, prelude::*, px};

use crate::markdown::parser::{BlockTree, parse_full};
use crate::markdown::render::{RenderCache, RenderOptions};
use crate::theme::Theme;

/// Which paths offer a preview mode — the extensions the sidebar treats as
/// Markdown. Pure; unit-tested.
pub(crate) fn is_markdown_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown"))
}

/// The preview's settled tree, or the transient state while a parse runs.
enum PreviewState {
    Empty,
    Parsing,
    Ready(Arc<BlockTree>),
}

pub struct MarkdownPreview {
    state: PreviewState,
    /// Bumped per submitted text; a parse for an older revision is dropped,
    /// never applied — preview work for an older revision cannot replace the
    /// latest rendered content.
    generation: u64,
    parse_task: Option<Task<()>>,
    /// The transcript's flatten cache pattern: settled blocks reuse their
    /// flat text + runs across frames.
    render_cache: Rc<RefCell<RenderCache>>,
    row_key: SharedString,
}

impl MarkdownPreview {
    pub fn new(row_key: SharedString, cx: &mut Context<Self>) -> Self {
        let mut preview = Self {
            state: PreviewState::Empty,
            generation: 0,
            parse_task: None,
            render_cache: Rc::new(RefCell::new(RenderCache::default())),
            row_key,
        };
        preview.set_text(String::new(), cx);
        preview
    }

    /// Submit the buffer to render. The parse runs off the UI thread; only
    /// the newest generation's result lands.
    pub fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        if text.is_empty() {
            self.state = PreviewState::Empty;
            self.parse_task = None;
            cx.notify();
            return;
        }
        self.state = PreviewState::Parsing;
        self.parse_task = Some(cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let tree = cx
                .background_executor()
                .spawn(async move { Arc::new(parse_full(&text)) })
                .await;
            let _ = this.update(cx, |preview, cx| {
                if preview.generation != generation {
                    return; // a newer revision superseded this parse
                }
                preview.state = PreviewState::Ready(tree);
                preview.parse_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn is_parsing(&self) -> bool {
        self.parse_task.is_some()
    }

    /// The rendered first-paragraph text — a test probe for "what is on
    /// screen" without a window.
    #[cfg(test)]
    pub(crate) fn first_paragraph(&self) -> Option<String> {
        match &self.state {
            PreviewState::Ready(tree) => tree.blocks.iter().find_map(|top| match &top.block {
                crate::markdown::parser::Block::Paragraph { runs } => {
                    Some(runs.iter().map(|run| run.text.as_str()).collect::<String>())
                }
                _ => None,
            }),
            _ => None,
        }
    }

    fn render_tree(&self, tree: &Arc<BlockTree>, theme: &Theme, window: &Window) -> AnyElement {
        let opts = RenderOptions {
            row_key: self.row_key.clone(),
            veil: None,
            cache: Some(self.render_cache.clone()),
            now: std::time::Instant::now(),
            copy: None,
        };
        div()
            .flex()
            .flex_col()
            .gap(px(crate::markdown::render::MD_BLOCK_GAP))
            .children(tree.blocks.iter().enumerate().map(|(ix, top)| {
                crate::markdown::render::render_block(
                    &top.block, ix, ix, &opts, theme, window, None,
                )
            }))
            .into_any_element()
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body: AnyElement = match &self.state {
            PreviewState::Empty => div()
                .py(px(48.0))
                .flex()
                .justify_center()
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child("Nothing to preview."),
                )
                .into_any_element(),
            PreviewState::Parsing => div()
                .py(px(48.0))
                .flex()
                .justify_center()
                .child(crate::loaders::mini_mono_spinner(
                    "file-md-parsing",
                    3.0,
                    crate::theme::ink(0.5),
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            PreviewState::Ready(tree) => self.render_tree(tree, &theme, window),
        };
        div()
            .id("file-md-preview")
            .size_full()
            .overflow_y_scroll()
            .px(px(20.0))
            .py(px(16.0))
            .text_size(px(crate::markdown::render::MD_TEXT_SIZE))
            .line_height(px(crate::markdown::render::MD_LINE_HEIGHT))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn markdown_paths_classify_by_extension_case_insensitively() {
        assert!(is_markdown_path("/notes/README.md"));
        assert!(is_markdown_path("/notes/Plan.MD"));
        assert!(is_markdown_path("/notes/notes.markdown"));
        assert!(!is_markdown_path("/notes/notes.txt"));
        assert!(!is_markdown_path("/notes/notes.mdx"));
        assert!(!is_markdown_path("/notes/png.md.exe"));
        assert!(!is_markdown_path("/notes/noext"));
    }

    #[gpui::test]
    fn parses_off_thread_and_renders_the_latest_revision_only(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let preview = cx.new(|cx| MarkdownPreview::new("preview-1".into(), cx));
        cx.run_until_parked();
        // The constructor's empty settle.
        preview.read_with(cx, |preview, _| {
            assert_eq!(preview.first_paragraph(), None);
            assert!(!preview.is_parsing());
        });

        preview.update(cx, |preview, cx| {
            preview.set_text("# Title\n\nfirst revision".into(), cx);
        });
        cx.run_until_parked();
        preview.read_with(cx, |preview, _| {
            assert_eq!(preview.first_paragraph().as_deref(), Some("first revision"));
            assert!(!preview.is_parsing());
        });

        // A superseding revision drops the in-flight parse's result: even if
        // an older parse finished LAST, the newer text wins.
        preview.update(cx, |preview, cx| {
            preview.set_text("second revision".into(), cx);
        });
        preview.update(cx, |preview, cx| {
            preview.set_text("third revision".into(), cx);
        });
        cx.run_until_parked();
        preview.read_with(cx, |preview, _| {
            assert_eq!(
                preview.first_paragraph().as_deref(),
                Some("third revision"),
                "an older revision's parse result must never replace the latest"
            );
        });

        // Empty text clears back to the explicit empty state.
        preview.update(cx, |preview, cx| {
            preview.set_text(String::new(), cx);
        });
        cx.run_until_parked();
        preview.read_with(cx, |preview, _| {
            assert_eq!(preview.first_paragraph(), None);
        });
    }
}
