//! The Turn file-change card (ADR-0024): one Turn's totals with Undo and
//! Review over a foldable file list. Separate from the generic row renderers —
//! the card is a Turn result with its own fold geometry (see `row_outer_pads`).

use super::*;

/// Files a Turn card lists before "Show more".
const TURN_CARD_VISIBLE_FILES: usize = 8;

impl Transcript {
    /// The Turn file-change card (ADR-0024 tickets 03+04): what one Turn
    /// changed, at the end of the Turn's reply. The card is a Turn RESULT:
    /// it only exists once the engine's Final frame settles the Turn — live
    /// frames never draw (user request), and a failed or interrupted settle
    /// is also a Final, so those Turns keep their cards. The header carries
    /// the totals with Undo and Review and folds the file list (open by
    /// default), which is capped at [`TURN_CARD_VISIBLE_FILES`] rows until
    /// "Show more". A row
    /// click opens the read-only review for that file; hovering a live
    /// file reveals Open — deleted files offer Review only.
    pub(super) fn render_turn_change_card(
        &mut self,
        row_id: &SharedString,
        change_set: &Arc<TurnChangeSet>,
        restore_mark: Option<TurnRestoreMark>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chat_id = self.chat_id.clone().unwrap_or_default();
        let message_id = change_set.message_id.clone();
        let open = self
            .folds
            .get(row_id)
            .and_then(|fold| fold.open)
            .unwrap_or(true);
        let more_id = SharedString::from(format!("{row_id}#more"));
        let show_all = self
            .folds
            .get(&more_id)
            .and_then(|fold| fold.open)
            .unwrap_or(false);
        let count = change_set.files.len();

        let tile = div()
            .flex_none()
            .size(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.hairline(0.10))
            .bg(theme.ink(0.04))
            .child(
                crate::icons::icon(crate::icons::DOCUMENT)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            );

        // A clean restore leaves the card as history: counts and paths fade.
        let undone = restore_mark == Some(TurnRestoreMark::Restored);
        let mark = restore_mark.map(|mark| match mark {
            TurnRestoreMark::Running => ("Restoring…", theme.text_faint),
            TurnRestoreMark::Restored => ("Undone", theme.text_muted),
            TurnRestoreMark::Partial => ("Partially restored", theme.warning),
            TurnRestoreMark::Failed => ("Restore failed", theme.danger),
        });
        let summary = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .when(change_set.additions > 0, |el| {
                el.child(turn_change_count(change_set.additions, true, undone, theme))
            })
            .when(change_set.deletions > 0, |el| {
                el.child(turn_change_count(
                    change_set.deletions,
                    false,
                    undone,
                    theme,
                ))
            })
            .when(change_set.truncated, |el| {
                el.child(
                    div()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.text_faint)
                        .child("diff truncated"),
                )
            })
            .when_some(mark, |el, (label, color)| {
                el.child(
                    div()
                        .debug_selector(|| "turn-card-restore-mark".to_string())
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(color)
                        .child(label),
                )
            });
        let title = div()
            .min_w_0()
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(18.0))
                            .text_color(theme.text)
                            .child(SharedString::from(format!(
                                "Edited {count} file{}",
                                if count == 1 { "" } else { "s" }
                            ))),
                    )
                    .child(
                        crate::icons::icon(if open {
                            crate::icons::ALT_ARROW_DOWN
                        } else {
                            crate::icons::ALT_ARROW_RIGHT
                        })
                        .flex_none()
                        .size(px(12.0))
                        .text_color(theme.text_faint),
                    ),
            )
            .child(summary);

        // Undo is offered on a settled card until a restore is running or
        // has landed cleanly; a partial or failed one keeps the retry.
        let can_undo = change_set.phase == holt_proto::TurnChangeSetPhase::Final
            && !matches!(
                restore_mark,
                Some(TurnRestoreMark::Running | TurnRestoreMark::Restored)
            );
        let restore_chat = chat_id.clone();
        let restore_message = message_id.clone();
        let undo = div()
            .id("turn-change-restore")
            .debug_selector(|| "turn-card-restore".to_string())
            .flex_none()
            .h(px(26.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(7.0))
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|el| el.bg(crate::theme::wash(0.06)))
            .child("Undo")
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(super::TranscriptEvent::RestoreTurnChanges {
                    chat_id: restore_chat.clone(),
                    message_id: restore_message.clone(),
                });
            }));
        let review_chat = chat_id.clone();
        let review_message = message_id.clone();
        let review = div()
            .id("turn-change-review")
            .debug_selector(|| "turn-card-review".to_string())
            .flex_none()
            .h(px(26.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.hairline(0.14))
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(theme.text)
            .cursor_pointer()
            .hover(|el| el.bg(crate::theme::wash(0.06)))
            .child("Review changes")
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(super::TranscriptEvent::ReviewTurnChanges {
                    chat_id: review_chat.clone(),
                    message_id: review_message.clone(),
                    path: None,
                });
            }));
        let toggle_id = row_id.clone();
        let header = div()
            .id("turn-change-toggle")
            .debug_selector(|| "turn-card-toggle".to_string())
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let fold = this.folds.entry(toggle_id.clone()).or_default();
                fold.open = Some(!fold.open.unwrap_or(true));
                cx.notify();
            }))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(10.0))
            .child(tile)
            .child(title)
            .when(can_undo, |el| el.child(undo))
            .child(review);

        let visible = if !open {
            0
        } else if show_all {
            count
        } else {
            count.min(TURN_CARD_VISIBLE_FILES)
        };
        let rows = change_set.files[..visible].iter().map(|file| {
            let review_chat = chat_id.clone();
            let review_message = message_id.clone();
            let review_path = file.path.clone();
            let row_selector = review_path.clone();
            let group = SharedString::from(format!("turn-change-row-{}", file.path));
            let open = (file.status != TurnFileChangeStatus::Deleted).then(|| {
                let open_path = file.path.clone();
                let open_selector = file.path.clone();
                div()
                    .id(SharedString::from(format!(
                        "turn-change-open-{}",
                        file.path
                    )))
                    .debug_selector(move || format!("turn-card-open-{open_selector}"))
                    .flex_none()
                    .h(px(20.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .rounded(px(5.0))
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .opacity(0.0)
                    .group_hover(group.clone(), |el| el.opacity(1.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(crate::theme::wash(0.08)))
                    .child("Open")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(super::TranscriptEvent::OpenTurnFile {
                            path: open_path.clone(),
                        });
                    }))
            });
            div()
                .id(SharedString::from(format!(
                    "turn-change-file-{}",
                    file.path
                )))
                .debug_selector(move || format!("turn-card-file-{row_selector}"))
                .group(group)
                .w_full()
                .h(px(34.0))
                .px(px(12.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .min_w_0()
                .border_t_1()
                .border_color(theme.hairline(0.08))
                .cursor_pointer()
                .hover(|el| el.bg(crate::theme::wash(0.04)))
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(super::TranscriptEvent::ReviewTurnChanges {
                        chat_id: review_chat.clone(),
                        message_id: review_message.clone(),
                        path: Some(review_path.clone()),
                    });
                }))
                .child(turn_change_file_path(file, undone, theme))
                .children(open)
                .child(turn_change_file_counts(file, undone, theme))
        });
        let hidden = count - visible;
        let more = (open && count > TURN_CARD_VISIBLE_FILES).then(|| {
            div()
                .id("turn-change-more")
                .debug_selector(|| "turn-card-more".to_string())
                .w_full()
                .h(px(30.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .border_t_1()
                .border_color(theme.hairline(0.08))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .cursor_pointer()
                .hover(|el| el.bg(crate::theme::wash(0.04)).text_color(theme.text_muted))
                .child(SharedString::from(if hidden > 0 {
                    format!("Show {hidden} more")
                } else {
                    "Show less".to_string()
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let fold = this.folds.entry(more_id.clone()).or_default();
                    fold.open = Some(!fold.open.unwrap_or(false));
                    cx.notify();
                }))
        });

        let card = div()
            .w_full()
            .max_w(px(720.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.hairline(0.12))
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .bg(theme.ink(0.02))
                    .children(rows)
                    .children(more),
            );
        div().py(px(4.0)).w_full().child(card).into_any_element()
    }
}

/// A Turn card row's path: a faint directory and the brighter file name
/// (a rename leads with `old → `). The directory gives way first, so a
/// long path keeps its file name readable.
fn turn_change_file_path(file: &TurnFileChange, undone: bool, theme: &Theme) -> gpui::Div {
    let (dir, name) = split_turn_path(&file.path);
    let lead = match &file.old_path {
        Some(old) => format!("{old} → {dir}"),
        None => dir.to_string(),
    };
    div()
        .min_w_0()
        .flex_1()
        .flex()
        .flex_row()
        .overflow_hidden()
        .font_family(theme.font_mono.clone())
        .text_size(crate::typography::ui_rems(12.5))
        .when(!lead.is_empty(), |el| {
            el.child(
                div()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .truncate()
                    .text_color(theme.text_faint)
                    .child(SharedString::from(lead)),
            )
        })
        .child(
            div()
                .min_w_0()
                .flex_shrink(0.01)
                .truncate()
                .text_color(if undone {
                    theme.text_faint
                } else {
                    theme.text_dim
                })
                .child(SharedString::from(name.to_string())),
        )
}

/// A row's `+n −n`, right-aligned together. A binary entry shows `BIN` —
/// never invented line counts.
fn turn_change_file_counts(file: &TurnFileChange, undone: bool, theme: &Theme) -> gpui::Div {
    let counts = div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0));
    if file.binary {
        return counts.child(
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_faint)
                .child("BIN"),
        );
    }
    counts
        .when(file.additions > 0, |el| {
            el.child(turn_change_count(file.additions, true, undone, theme))
        })
        .when(file.deletions > 0, |el| {
            el.child(turn_change_count(file.deletions, false, undone, theme))
        })
}

/// `src/ui/app.rs` → (`src/ui/`, `app.rs`); a root file has no directory.
fn split_turn_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(ix) => path.split_at(ix + 1),
        None => ("", path),
    }
}

/// A `+n`/`−n` count in the diff palette — the Changes pane's header idiom;
/// faint once the Turn is undone.
fn turn_change_count(count: u32, added: bool, undone: bool, theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .font_family(theme.font_mono.clone())
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(if undone {
            theme.text_faint
        } else if added {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_paths_split_into_directory_and_file_name() {
        assert_eq!(split_turn_path("src/ui/app.rs"), ("src/ui/", "app.rs"));
        assert_eq!(split_turn_path("Cargo.toml"), ("", "Cargo.toml"));
    }
}
