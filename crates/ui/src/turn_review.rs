//! The Turn review surface (ADR-0024, ticket 04): the right pane's
//! read-only per-file unified diff over one Turn's change set. The data is
//! `GetCheckoutFileDiffText` in `turn` mode addressed by the Turn's message
//! id — a settled Turn serves its immutable persisted before/after pair, the
//! live current Turn reads the working tree against its baseline. The one
//! write is Restore (`RestoreTurnChanges`): confirmed inline, then file I/O
//! in the engine; no accept/stage/commit.

use gpui::{AnyElement, Context, Render, SharedString, div, prelude::*, px};
use holt_proto::{TurnFileChange, TurnFileChangeStatus};

use crate::changes::{
    ACCENT_BAR_WIDTH, DIFF_TEXT_SIZE, FileDiff, FileStatus, LineKind, MARKER_WIDTH, gutter_width,
    render_file_body_with_syntax,
};
use crate::state::AppState;
use crate::theme::Theme;

/// What one aimed file's read produced. The variant's `path` plus the
/// entity's `generation` together make a superseded aim's late reply
/// unrepresentable: the guards in `fail`/`apply_reply` drop it, and render
/// only ever sees the current aim's state.
enum ReviewLoad {
    /// A fresh aim with no path yet — the caller always picks one.
    Idle,
    Loading {
        path: String,
    },
    Loaded {
        path: String,
        file: FileDiff,
        /// The change set's Git-derived counts — the text pair can be
        /// truncated, so its own recount may undercount what the card shows.
        additions: u32,
        deletions: u32,
        truncated: bool,
    },
    Binary {
        path: String,
    },
    Failed {
        path: String,
        error: SharedString,
    },
}

/// The restore flow: confirm dialog, then an inline running/result line.
enum RestoreUi {
    Idle,
    /// `None` restores the whole change set.
    Confirm {
        path: Option<String>,
    },
    Running,
    Done(SharedString),
}

/// One Turn's review surface. A single companion per shell: a new
/// file selection RE-aims this entity (replacing the active view), never
/// stacks tabs. The file list is captured at aim time from the change set —
/// final sets are frozen in the store, so only a live Turn's review can go
/// stale, and re-clicking refreshes it.
pub struct TurnReview {
    state: gpui::Entity<AppState>,
    chat_id: String,
    message_id: String,
    /// The Turn's changed files, captured at aim — the card is the live
    /// index; this is the review's stable one.
    files: Vec<TurnFileChange>,
    load: ReviewLoad,
    restore: RestoreUi,
    /// Memoized scroll-content width of the loaded body (the text system is
    /// window-scoped, so measuring happens lazily at render); cleared per aim.
    content_width: Option<gpui::Pixels>,
    generation: u64,
    fetch: Option<gpui::Task<()>>,
}

impl TurnReview {
    /// A fresh, idle surface; `aim` gives it a Turn.
    pub fn new(state: gpui::Entity<AppState>) -> Self {
        Self {
            state,
            chat_id: String::new(),
            message_id: String::new(),
            files: Vec::new(),
            load: ReviewLoad::Idle,
            restore: RestoreUi::Idle,
            content_width: None,
            generation: 0,
            fetch: None,
        }
    }

    /// Aim the review at one Turn: capture the change set's file list and
    /// select `path` (the first file when unspecified). A re-aim replaces
    /// whatever was showing.
    pub fn aim(
        &mut self,
        chat_id: &str,
        message_id: &str,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.chat_id = chat_id.to_string();
        self.message_id = message_id.to_string();
        self.restore = RestoreUi::Idle;
        self.files = self
            .state
            .read(cx)
            .turn_change_sets
            .get(message_id)
            .map(|set| set.files.clone())
            .unwrap_or_default();
        match path.or_else(|| self.files.first().map(|file| file.path.clone())) {
            Some(path) => self.select(path, cx),
            None => {
                self.load = ReviewLoad::Idle;
                cx.notify();
            }
        }
    }

    /// Select one of the Turn's files and fetch its diff text.
    fn select(&mut self, path: String, cx: &mut Context<Self>) {
        self.generation += 1;
        self.load = ReviewLoad::Loading { path: path.clone() };
        self.content_width = None;
        cx.notify();
        self.fetch = Some(self.spawn_fetch(path, self.generation, cx));
    }

    /// The read behind the review — one `GetCheckoutFileDiffText` in turn
    /// mode, addressed by the Turn's message id. Generation-guarded so a
    /// superseded aim's reply lands nowhere.
    fn spawn_fetch(&self, path: String, generation: u64, cx: &mut Context<Self>) -> gpui::Task<()> {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.fail(path, generation, "the engine is not connected", cx)
                });
            });
        };
        let Some(cwd) = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == self.chat_id)
            .and_then(|chat| chat.cwd.clone())
        else {
            return cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.fail(path, generation, "the chat has no working directory", cx)
                });
            });
        };
        let chat_id = self.chat_id.clone();
        let message_id = self.message_id.clone();
        cx.spawn(async move |this, cx| {
            let request = holt_proto::GetCheckoutFileDiffTextRequest {
                checkout_id: String::new(),
                cwd,
                path: path.clone(),
                mode: crate::changes::DiffScope::LatestTurn.mode().to_string(),
                base_ref: None,
                chat_id: Some(chat_id),
                commit_sha: None,
                message_id: Some(message_id),
                diff_checksum: String::new(),
            };
            let params = serde_json::to_value(request)
                .ok()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            let reply = engine.client().call(
                holt_rpc::methods::GET_CHECKOUT_FILE_DIFF_TEXT,
                serde_json::Value::Object(params),
            );
            let Ok(value) = reply.await else {
                let _ = this.update(cx, |this, cx| {
                    this.fail(path, generation, "the diff could not be read", cx)
                });
                return;
            };
            let Ok(text) = serde_json::from_value::<holt_proto::CheckoutFileDiffText>(value) else {
                let _ = this.update(cx, |this, cx| {
                    this.fail(path, generation, "malformed diff reply", cx)
                });
                return;
            };
            let _ = this.update(cx, |this, cx| this.apply_reply(path, generation, text, cx));
        })
    }

    fn fail(&mut self, path: String, generation: u64, error: &str, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.load = ReviewLoad::Failed {
            path,
            error: SharedString::from(error),
        };
        cx.notify();
    }

    fn apply_reply(
        &mut self,
        path: String,
        generation: u64,
        text: holt_proto::CheckoutFileDiffText,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        let file = self
            .files
            .iter()
            .find(|file| file.path == path)
            .expect("selected from this.files");
        self.load = if text.binary {
            ReviewLoad::Binary { path }
        } else {
            ReviewLoad::Loaded {
                path: path.clone(),
                file: text_pair_to_file(file, &text),
                additions: file.additions,
                deletions: file.deletions,
                truncated: text.truncated,
            }
        };
        cx.notify();
    }

    /// Ask to restore one file (`Some`) or the whole set (`None`); nothing is
    /// written until the user confirms.
    pub(crate) fn request_restore(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if !matches!(self.restore, RestoreUi::Running) {
            self.restore = RestoreUi::Confirm { path };
            cx.notify();
        }
    }

    fn run_restore(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.restore = RestoreUi::Done("The engine is not connected.".into());
            cx.notify();
            return;
        };
        self.restore = RestoreUi::Running;
        cx.notify();
        let (chat_id, message_id) = (self.chat_id.clone(), self.message_id.clone());
        cx.spawn(async move |this, cx| {
            let (message, _) = restore_call(engine, chat_id, message_id, path).await;
            let _ = this.update(cx, |this, cx| {
                // Only a still-Running flow owns the reply: a re-aim resets
                // the state mid-flight, and a late Done must not stomp the
                // fresh one (the fetch path's generation rule, restated).
                if matches!(this.restore, RestoreUi::Running) {
                    this.restore = RestoreUi::Done(message.into());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Retry the failed read (the image-viewer idiom): a failed review is a
    /// failed fetch, and the fix is the same fetch again.
    pub(crate) fn retry(&mut self, cx: &mut Context<Self>) {
        let path = match &self.load {
            ReviewLoad::Failed { path, .. } => path.clone(),
            _ => return,
        };
        self.select(path, cx);
    }

    /// The failed read's message — test/observation hook for the retry path.
    #[cfg(test)]
    pub(crate) fn failed_error(&self) -> Option<SharedString> {
        match &self.load {
            ReviewLoad::Failed { error, .. } => Some(error.clone()),
            _ => None,
        }
    }

    /// The surface header's muted companion: what the review is looking at.
    pub fn header_path(&self) -> SharedString {
        let path = match &self.load {
            ReviewLoad::Idle => return SharedString::from("no file selected"),
            ReviewLoad::Loading { path, .. }
            | ReviewLoad::Loaded { path, .. }
            | ReviewLoad::Binary { path, .. }
            | ReviewLoad::Failed { path, .. } => path,
        };
        SharedString::from(path.clone())
    }
}

/// Run `RestoreTurnChanges` for one file (`Some`) or the whole set and
/// phrase the outcome; the flag is true when nothing was refused or failed.
pub(crate) async fn restore_call(
    engine: crate::state::EngineHandle,
    chat_id: String,
    message_id: String,
    path: Option<String>,
) -> (String, bool) {
    let params = serde_json::json!({
        "chatId": chat_id,
        "messageId": message_id,
        "paths": path.into_iter().collect::<Vec<_>>(),
    });
    let reply = engine
        .client()
        .call(holt_rpc::methods::RESTORE_TURN_CHANGES, params)
        .await;
    match reply {
        Err(error) => (format!("Restore failed: {error}"), false),
        Ok(value) => match serde_json::from_value::<holt_proto::TurnRestoreReply>(value) {
            Ok(reply) => {
                let clean = reply.files.iter().all(|file| {
                    !matches!(file.outcome, holt_proto::TurnRestoreOutcome::Refused { .. })
                });
                (restore_summary(&reply), clean)
            }
            Err(_) => ("Restore failed: malformed reply".to_string(), false),
        },
    }
}

/// The confirm dialog's title and body for restoring `path` (or, with
/// `None`, every file of `files`).
pub(crate) fn restore_question(
    files: &[TurnFileChange],
    path: Option<&str>,
) -> (&'static str, String) {
    match path {
        // A rename restores to the OLD path and removes the current one —
        // say so before the write.
        Some(path) => {
            let old = files
                .iter()
                .find(|file| file.path == path)
                .and_then(|file| file.old_path.clone());
            match old {
                Some(old) => (
                    "Restore file?",
                    format!(
                        "Move {path} back to {old} with its pre-Turn content? The file at {path} will be removed."
                    ),
                ),
                None => (
                    "Restore file?",
                    format!("{path} will be restored to its content before this Turn."),
                ),
            }
        }
        None => (
            "Restore all files?",
            format!(
                "{} file{} will be restored to their content before this Turn.{}",
                files.len(),
                if files.len() == 1 { "" } else { "s" },
                if files.iter().any(|file| file.old_path.is_some()) {
                    " Moved files return to their pre-Turn paths."
                } else {
                    ""
                }
            ),
        ),
    }
}

/// The restore confirm modal, shared by the Turn card (hosted by the shell)
/// and the review pane. The caller wires cancel (scrim click and button) and
/// confirm.
pub(crate) fn restore_dialog(
    theme: &Theme,
    viewport: gpui::Size<gpui::Pixels>,
    title: &str,
    question: String,
    cancel_out: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    cancel: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    confirm: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let card = crate::popover::dialog_card(theme)
        .on_mouse_down_out(cancel_out)
        .child(crate::popover::dialog_title(theme, title))
        .child(
            div()
                .mt(px(6.0))
                .child(crate::popover::dialog_body(theme, question)),
        )
        .child(
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(
                    crate::popover::btn_ghost(theme, "Cancel", "turn-review-restore-cancel")
                        .id("turn-review-restore-cancel")
                        .debug_selector(|| "turn-review-restore-cancel".to_string())
                        .on_click(cancel),
                )
                .child(
                    crate::popover::btn_primary(theme, "Restore")
                        .id("turn-review-restore-confirm")
                        .debug_selector(|| "turn-review-restore-confirm".to_string())
                        .on_click(confirm),
                ),
        )
        .into_any_element();
    crate::popover::modal("turn-review-restore-dialog", viewport, card)
}

fn refusal_text(reason: &holt_proto::TurnRestoreRefusal) -> String {
    use holt_proto::TurnRestoreRefusal::*;
    match reason {
        Conflict => "changed since the Turn".into(),
        LaterTurn => "changed by a later Turn".into(),
        Truncated => "stored content is truncated".into(),
        Binary => "binary file".into(),
        LossyText => "not valid UTF-8".into(),
        UnsafePath => "path outside the working tree".into(),
        Io { message } => message.clone(),
    }
}

fn restore_summary(reply: &holt_proto::TurnRestoreReply) -> String {
    use holt_proto::TurnRestoreOutcome::*;
    let restored = reply
        .files
        .iter()
        .filter(|file| file.outcome == Restored)
        .count();
    let already = reply
        .files
        .iter()
        .filter(|file| file.outcome == AlreadyRestored)
        .count();
    let mut text = format!(
        "Restored {restored} file{}.",
        if restored == 1 { "" } else { "s" }
    );
    if already > 0 {
        text.push_str(&format!(" {already} already restored."));
    }
    for file in &reply.files {
        if let Refused { reason } = &file.outcome {
            text.push_str(&format!(" {}: {}.", file.path, refusal_text(reason)));
        }
    }
    text
}

/// The scroll content's minimum width: row chrome plus the widest rendered
/// text (diff lines at the body size, meta notes / hunk headers / notices at
/// theirs), so an over-long line scrolls into view horizontally instead of
/// clipping at the pane edge. Measured against the window's text system.
fn body_content_width(file: &FileDiff, theme: &Theme, window: &gpui::Window) -> gpui::Pixels {
    let font = gpui::font(theme.font_mono.clone());
    let text_system = window.text_system();
    let width_at = |text: &str, size: f32| -> f32 {
        let run = gpui::TextRun {
            len: text.len(),
            font: font.clone(),
            color: theme.text,
            ..Default::default()
        };
        text_system
            .layout_line(text, px(size), &[run], None)
            .width
            .into()
    };
    // The diff row's fixed columns: accent bar, both gutters, the +/− marker,
    // and the text's left padding (meta rows fold the same total into their
    // left padding).
    let chrome = ACCENT_BAR_WIDTH + 2.0 * gutter_width(file) + MARKER_WIDTH + 12.0;
    let lines = file
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .map(|line| {
            let size = if line.kind == LineKind::Meta {
                10.5
            } else {
                DIFF_TEXT_SIZE
            };
            chrome + width_at(&line.text, size)
        });
    let headers = file
        .hunks
        .iter()
        .map(|hunk| 2.0 * Theme::SPACE_LG + width_at(&hunk.header, 11.0));
    let notices = file
        .notices
        .iter()
        .map(|notice| 2.0 * Theme::SPACE_LG + width_at(notice, 11.0));
    let widest = lines.chain(headers).chain(notices).fold(0.0_f32, f32::max);
    // Trailing slack so the last glyph never sits flush against the edge.
    px(widest + 8.0)
}

impl Render for TurnReview {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // Measure the loaded body once (the text system is window-scoped) so
        // the scroll content can widen past the pane for long lines.
        let content_width = if let ReviewLoad::Loaded { file, .. } = &self.load {
            Some(match self.content_width {
                Some(width) => width,
                None => {
                    let width = body_content_width(file, &theme, window);
                    self.content_width = Some(width);
                    width
                }
            })
        } else {
            None
        };
        let can_restore = self
            .state
            .read(cx)
            .turn_change_sets
            .get(&self.message_id)
            .is_some_and(|set| set.phase == holt_proto::TurnChangeSetPhase::Final);
        let body = match &self.load {
            ReviewLoad::Idle => {
                centered_note("Nothing to review — the Turn changed no files.", &theme)
            }
            ReviewLoad::Loading { path, .. } => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .pt(px(48.0))
                .child(crate::loaders::mini_mono_spinner(
                    "turn-review-loading",
                    2.0,
                    theme.text_muted,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(format!("Reviewing {path}…"))),
                )
                .into_any_element(),
            ReviewLoad::Binary { .. } => centered_note(
                "Binary file — its content has no text diff. Status and size changes live on the card.",
                &theme,
            ),
            ReviewLoad::Failed { error, .. } => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.0))
                .pt(px(48.0))
                .child(
                    crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                        .size(px(16.0))
                        .text_color(theme.danger.opacity(0.8)),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(error.clone()),
                )
                .child(
                    div()
                        .id("turn-review-retry")
                        .px(px(10.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|el| el.bg(crate::theme::wash(0.06)))
                        .child("Retry")
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.retry(cx);
                        })),
                )
                .into_any_element(),
            ReviewLoad::Loaded {
                file,
                additions,
                deletions,
                truncated,
                ..
            } => div()
                // size_full is load-bearing: without a definite height the
                // column grows to the diff's content height, the scroll
                // region's `flex_1` has nothing to distribute, and the pane
                // can never scroll (extent stays zero).
                .size_full()
                .flex()
                .flex_col()
                .child(
                    // The file's summary bar: status letter, path, counts —
                    // the card row's idiom, restated where the diff lives.
                    div()
                        .flex_none()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(12.0))
                        .py(px(8.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(status_color(file.status, &theme))
                                .child(status_letter(file.status)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(12.0))
                                .text_color(theme.text_dim)
                                .child(SharedString::from(match &file.old_path {
                                    Some(old) => format!("{old} → {}", file.path),
                                    None => file.path.clone(),
                                })),
                        )
                        .when(*additions > 0, |el| {
                            el.child(count(*additions, true, &theme))
                        })
                        .when(*deletions > 0, |el| {
                            el.child(count(*deletions, false, &theme))
                        })
                        .when(*truncated, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_faint)
                                    .child("content truncated"),
                            )
                        })
                        .when(can_restore && !*truncated, |el| {
                            let restore_path = file.path.clone();
                            el.child(
                                div()
                                    .id("turn-review-restore-file")
                                    .debug_selector(|| "turn-review-restore-file".into())
                                    .flex_none()
                                    .px(px(7.0))
                                    .rounded(px(5.0))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .cursor_pointer()
                                    .hover(|el| el.bg(crate::theme::wash(0.06)))
                                    .child("Restore")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.request_restore(Some(restore_path.clone()), cx);
                                    })),
                            )
                        }),
                )
                .child(
                    // One file's read-only unified diff, the shared body
                    // renderer (the same one behind the transcript's tool
                    // diffs).
                    div()
                        .id("turn-review-scroll")
                        .debug_selector(|| "turn-review-scroll".into())
                        .flex_1()
                        .min_h_0()
                        .overflow_scroll()
                        .py(px(8.0))
                        .child(
                            div()
                                .debug_selector(|| "turn-review-content".into())
                                .min_w(content_width.unwrap_or_default())
                                .child(render_file_body_with_syntax(file, None, &theme)),
                        ),
                )
                .into_any_element(),
        };
        let bar = match &self.restore {
            RestoreUi::Idle => None,
            RestoreUi::Running => Some(div().child("Restoring…")),
            RestoreUi::Done(message) => Some(div().child(message.clone())),
            RestoreUi::Confirm { .. } => None,
        };
        let dialog = match &self.restore {
            RestoreUi::Confirm { path } => {
                let (title, question) = restore_question(&self.files, path.as_deref());
                let path = path.clone();
                Some(restore_dialog(
                    &theme,
                    window.viewport_size(),
                    title,
                    question,
                    cx.listener(|this, _, _, cx| {
                        this.restore = RestoreUi::Idle;
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.restore = RestoreUi::Idle;
                        cx.notify();
                    }),
                    cx.listener(move |this, _, _, cx| this.run_restore(path.clone(), cx)),
                ))
            }
            _ => None,
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(bar, |el, bar| {
                el.child(
                    bar.flex_none()
                        .w_full()
                        .px(px(12.0))
                        .py(px(6.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(11.5))
                        .text_color(theme.text_dim),
                )
            })
            .child(div().flex_1().min_h_0().w_full().child(body))
            .children(dialog)
    }
}

fn centered_note(message: &str, theme: &Theme) -> AnyElement {
    div()
        .w_full()
        .pt(px(48.0))
        .flex()
        .justify_center()
        .child(
            div()
                .max_w(px(320.0))
                .text_center()
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(theme.text_muted.opacity(0.9))
                .child(SharedString::from(message)),
        )
        .into_any_element()
}

fn status_letter(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "A",
        FileStatus::Modified => "M",
        FileStatus::Deleted => "D",
        FileStatus::Renamed => "R",
    }
}

fn status_color(status: FileStatus, theme: &Theme) -> gpui::Hsla {
    match status {
        FileStatus::Added => theme.success,
        FileStatus::Modified => theme.warning,
        FileStatus::Deleted => theme.danger,
        FileStatus::Renamed => theme.accent,
    }
}

fn count(count: u32, added: bool, theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .font_family(theme.font_mono.clone())
        .text_size(px(11.0))
        .text_color(if added {
            theme.diff_add
        } else {
            theme.diff_del
        })
        .child(SharedString::from(if added {
            format!("+{count}")
        } else {
            format!("−{count}")
        }))
}

/// Build the read-only review's one-file diff from a Turn file's status plus
/// its fetched before/after text pair — the shared hunk reduction
/// ([`crate::changes::file_diff_from_text`]) over the Turn vocabulary: the
/// change set's status is authoritative (a delete pairs `old` with a missing
/// new side; a rename keeps its pre-move path for the header).
pub(crate) fn text_pair_to_file(
    file: &TurnFileChange,
    text: &holt_proto::CheckoutFileDiffText,
) -> FileDiff {
    let status = match file.status {
        TurnFileChangeStatus::Added => FileStatus::Added,
        TurnFileChangeStatus::Modified => FileStatus::Modified,
        TurnFileChangeStatus::Deleted => FileStatus::Deleted,
        TurnFileChangeStatus::Renamed => FileStatus::Renamed,
    };
    crate::changes::file_diff_from_text(
        &file.path,
        file.old_path.clone(),
        status,
        text.old_text.as_deref().unwrap_or(""),
        text.new_text.as_deref().unwrap_or(""),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(status: TurnFileChangeStatus, path: &str, old_path: Option<&str>) -> TurnFileChange {
        TurnFileChange {
            path: path.into(),
            old_path: old_path.map(str::to_string),
            status,
            additions: 0,
            deletions: 0,
            binary: false,
        }
    }

    fn text(old: Option<&str>, new: Option<&str>) -> holt_proto::CheckoutFileDiffText {
        holt_proto::CheckoutFileDiffText {
            diff_checksum: String::new(),
            old_text: old.map(str::to_string),
            new_text: new.map(str::to_string),
            old_content_hash: None,
            new_content_hash: None,
            binary: false,
            truncated: false,
            stale: false,
        }
    }

    #[test]
    fn a_modified_pair_builds_hunks_with_dual_numbers() {
        let file = text_pair_to_file(
            &file(TurnFileChangeStatus::Modified, "src/lib.rs", None),
            &text(Some("one\ntwo\nthree\n"), Some("one\nTWO\nthree\n")),
        );
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!(file.additions, 1);
        assert_eq!(file.deletions, 1);
        let hunk = &file.hunks[0];
        assert_eq!(hunk.header, "@@ -1,3 +1,3 @@");
        assert_eq!(hunk.lines[0].text, "one");
        // The deleted line keeps its OLD number; the inserted one its NEW.
        assert_eq!(hunk.lines[1].old_no, Some(2));
        assert_eq!(hunk.lines[1].new_no, None);
        assert_eq!(hunk.lines[1].text, "two");
        assert_eq!(hunk.lines[2].old_no, None);
        assert_eq!(hunk.lines[2].new_no, Some(2));
        assert_eq!(hunk.lines[2].text, "TWO");
    }

    #[test]
    fn added_and_deleted_pairs_keep_the_change_set_status() {
        let added = text_pair_to_file(
            &file(TurnFileChangeStatus::Added, "new.txt", None),
            &text(None, Some("fresh\n")),
        );
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.additions, 1);
        assert!(
            added.hunks[0]
                .lines
                .iter()
                .all(|line| line.old_no.is_none())
        );

        // The deleted-file review path (story 11): the old side alone.
        let deleted = text_pair_to_file(
            &file(TurnFileChangeStatus::Deleted, "gone.txt", None),
            &text(Some("vanished\n"), None),
        );
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert_eq!(deleted.deletions, 1);
        assert!(
            deleted.hunks[0]
                .lines
                .iter()
                .all(|line| line.new_no.is_none())
        );
    }

    #[test]
    fn a_rename_keeps_its_pre_move_path_for_the_header() {
        let file = text_pair_to_file(
            &file(TurnFileChangeStatus::Renamed, "moved.txt", Some("old.txt")),
            &text(Some("same\n"), Some("same\n")),
        );
        assert_eq!(file.status, FileStatus::Renamed);
        assert_eq!(file.old_path.as_deref(), Some("old.txt"));
        assert_eq!(file.path, "moved.txt");
    }

    /// The entity lifecycle without an engine: aim selects (the named file,
    /// else the first), a failed read surfaces its error, and retry re-runs
    /// the same fetch — the deterministic no-engine failure stands in for
    /// any RPC failure.
    #[gpui::test]
    fn a_aimed_review_fails_gracefully_and_retries(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let state = cx.new(|_| AppState::new());
        let change_set = holt_proto::TurnChangeSet {
            chat_id: "chat-1".into(),
            message_id: "m-1".into(),
            phase: holt_proto::TurnChangeSetPhase::Final,
            files: vec![
                file(TurnFileChangeStatus::Added, "a.txt", None),
                file(TurnFileChangeStatus::Deleted, "gone.txt", None),
            ],
            additions: 0,
            deletions: 0,
            truncated: false,
            updated_at: chrono::Utc::now(),
        };
        state.update(cx, |s, _| {
            s.turn_change_sets.insert("m-1".into(), change_set);
        });
        let review = cx.new(|_| TurnReview::new(state.clone()));

        // No path named: the first file is selected.
        review.update(cx, |review, cx| review.aim("chat-1", "m-1", None, cx));
        assert_eq!(
            review
                .read_with(cx, |review, _| review.header_path())
                .as_ref(),
            "a.txt"
        );
        cx.run_until_parked();
        let error = review
            .update(cx, |review, _| review.failed_error())
            .expect("the read fails without an engine");
        assert!(!error.is_empty());

        // A named path re-aims; the deleted file reviews like any other.
        review.update(cx, |review, cx| {
            review.aim("chat-1", "m-1", Some("gone.txt".into()), cx)
        });
        assert_eq!(
            review
                .read_with(cx, |review, _| review.header_path())
                .as_ref(),
            "gone.txt"
        );

        // Retry re-runs the fetch — still engine-less, still the failed
        // state, never a panic or a stuck spinner.
        cx.run_until_parked();
        review.update(cx, |review, cx| review.retry(cx));
        cx.run_until_parked();
        assert!(review.read_with(cx, |review, _| review.failed_error().is_some()));
    }

    /// The Loaded body's scroll region must be clamped to the pane: the
    /// container column needs a definite height, otherwise it grows to the
    /// diff's content height and the `overflow_y_scroll` child's extent
    /// stays zero — the pane could never scroll (user report).
    #[gpui::test]
    fn the_loaded_scroll_region_is_clamped_to_the_pane(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.update(|cx| cx.new(|_| AppState::new()));
        let added: String = (0..400).fold(String::new(), |acc, ix| acc + &format!("line {ix}\n"));
        let tall = text_pair_to_file(
            &file(TurnFileChangeStatus::Added, "big.txt", None),
            &text(None, Some(&added)),
        );
        let (_review, cx) = cx.add_window_view(move |_, _| {
            let mut review = TurnReview::new(state.clone());
            review.load = ReviewLoad::Loaded {
                path: "big.txt".into(),
                file: tall,
                additions: 400,
                deletions: 0,
                truncated: false,
            };
            review
        });
        cx.run_until_parked();

        let scroll = cx
            .debug_bounds("turn-review-scroll")
            .expect("the scroll region renders");
        let viewport = cx.update(|window, _| window.viewport_size());
        assert!(
            scroll.bottom() <= viewport.height + px(1.0),
            "the scroll region is clamped to the pane, not grown to the diff: {:?} vs {:?}",
            scroll,
            viewport,
        );
        assert!(
            scroll.size.height > px(100.0),
            "and it fills the pane below the summary bar"
        );
    }

    /// A line wider than the pane must widen the scroll CONTENT (not the
    /// scroll region), which is what gives the two-axis scroller its
    /// horizontal extent.
    #[gpui::test]
    fn a_wide_line_creates_horizontal_scroll_extent(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.update(|cx| cx.new(|_| AppState::new()));
        let wide_line = "x".repeat(600);
        let new_text = format!("short\n{wide_line}\nshort\n");
        let wide = text_pair_to_file(
            &file(TurnFileChangeStatus::Added, "wide.txt", None),
            &text(None, Some(&new_text)),
        );
        let (_review, cx) = cx.add_window_view(move |_, _| {
            let mut review = TurnReview::new(state.clone());
            review.load = ReviewLoad::Loaded {
                path: "wide.txt".into(),
                file: wide,
                additions: 3,
                deletions: 0,
                truncated: false,
            };
            review
        });
        cx.run_until_parked();

        let scroll = cx
            .debug_bounds("turn-review-scroll")
            .expect("the scroll region renders");
        let content = cx
            .debug_bounds("turn-review-content")
            .expect("the scroll content renders");
        assert!(
            content.size.width > scroll.size.width,
            "the wide line widens the content past the viewport: {:?} vs {:?}",
            content,
            scroll,
        );
        // Sanity: the measured width covers the 600-char line, so scrolling
        // to the right edge reveals all of it (12px mono ≈ 7px per glyph).
        assert!(content.size.width > px(3000.0));
    }
}
