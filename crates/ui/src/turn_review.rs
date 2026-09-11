//! The Turn review surface (ADR-0024, ticket 04): the right pane's
//! read-only per-file unified diff over one Turn's change set. The data is
//! `GetCheckoutFileDiffText` in `turn` mode addressed by the Turn's message
//! id — a settled Turn serves its immutable persisted before/after pair, the
//! live current Turn reads the working tree against its baseline. Deliberately
//! no accept/undo/stage/commit: the review reads, nothing more.

use gpui::{AnyElement, Context, Render, SharedString, div, prelude::*, px};
use holt_proto::{TurnFileChange, TurnFileChangeStatus};

use crate::changes::{FileDiff, FileStatus, render_file_body_with_syntax};
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

/// One Turn's read-only review surface. A single companion per shell: a new
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

impl Render for TurnReview {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
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
                .w_full()
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
                        }),
                )
                .child(
                    // One file's read-only unified diff, the shared body
                    // renderer (the same one behind the transcript's tool
                    // diffs).
                    div()
                        .id("turn-review-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .py(px(8.0))
                        .child(render_file_body_with_syntax(file, None, &theme)),
                )
                .into_any_element(),
        };
        div().size_full().flex().flex_col().child(body)
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
}
