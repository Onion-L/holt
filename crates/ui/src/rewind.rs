//! Rewind to a user message (ADR-rewind-to-a-message-is-all-or-nothing):
//! a user row's rewind affordance asks the engine to plan the cut
//! (`RewindToMessage`), confirms that plan in a dialog over the
//! conversation, then applies it (`ConfirmRewind`). A plan with conflicts
//! cannot be confirmed — the dialog only says why.

use std::borrow::Cow;

use gpui::{
    AnyElement, AppContext as _, InteractiveElement as _, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, div, px,
};
use holt_proto::{RewindAction, RewindConflictReason, RewindReply};

use crate::i18n::t;
use crate::theme::Theme;

/// The message a pending confirm dialog would rewind to, with its plan
/// and the row's raw text (the apply refills the composer draft from it —
/// after the cut the conversation no longer holds the message to read it
/// back from).
#[derive(Clone)]
pub(crate) struct RewindConfirm {
    pub chat_id: String,
    pub message_text: String,
    pub plan: RewindReply,
}

/// Plan a rewind with `RewindToMessage`; `Err` carries a phrased failure.
pub(crate) async fn rewind_call(
    engine: crate::state::EngineHandle,
    chat_id: String,
    message_id: String,
) -> Result<RewindReply, String> {
    let params = serde_json::json!({
        "chatId": chat_id,
        "messageId": message_id,
    });
    let value = engine
        .client()
        .call(holt_rpc::methods::REWIND_TO_MESSAGE, params)
        .await
        .map_err(|error| {
            t!(
                "conversation_transcript.rewind.failed",
                error = error.to_string()
            )
            .into_owned()
        })?;
    serde_json::from_value(value)
        .map_err(|_| t!("conversation_transcript.rewind.malformed").into_owned())
}

/// Apply the staged plan with `ConfirmRewind`. A reply that did not apply
/// carries the conflicts that refused it — nothing was written; `Err`
/// carries the engine's raw refusal phrasing (the caller maps the stable
/// ones to catalog copy and wraps the rest).
pub(crate) async fn confirm_rewind_call(
    engine: crate::state::EngineHandle,
    chat_id: String,
) -> Result<RewindReply, String> {
    let params = serde_json::json!({ "chatId": chat_id });
    let value = engine
        .client()
        .call(holt_rpc::methods::CONFIRM_REWIND, params)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_value(value)
        .map_err(|_| t!("conversation_transcript.rewind.malformed").into_owned())
}

/// Files listed in the dialog before the rest is counted.
const LISTED: usize = 8;

fn listed(lines: Vec<String>) -> String {
    let more = lines.len().saturating_sub(LISTED);
    let mut text = lines
        .into_iter()
        .take(LISTED)
        .collect::<Vec<_>>()
        .join("\n");
    if more > 0 {
        text.push_str(&format!(
            "\n{}",
            t!("conversation_transcript.rewind.and_more", count = more)
        ));
    }
    text
}

/// The dialog's opening line: how much conversation the cut removes —
/// the reply's own summary, not a fixed sentence.
fn removed_intro(turns: usize) -> Cow<'static, str> {
    if turns == 1 {
        t!("conversation_transcript.rewind.removed_intro.one")
    } else {
        t!(
            "conversation_transcript.rewind.removed_intro.other",
            count = turns
        )
    }
}

/// The undo scope's own line, when the plan undoes any recorded step.
fn steps_line(steps: usize) -> Option<Cow<'static, str>> {
    (steps > 0).then(|| {
        if steps == 1 {
            t!(
                "conversation_transcript.rewind.steps_undone.one",
                count = steps
            )
        } else {
            t!(
                "conversation_transcript.rewind.steps_undone.other",
                count = steps
            )
        }
    })
}

/// The stop line: a live Turn is interrupted by the confirm — the plan
/// says so up front, because canceling the dialog must be the user's way
/// out of that.
fn stop_line(will_stop: bool) -> Option<Cow<'static, str>> {
    will_stop.then(|| t!("conversation_transcript.rewind.stop_notice"))
}

/// The sweep line: queued prompts point at a conversation the cut removes,
/// so the confirm deletes them with it.
fn pending_line(count: u32) -> Option<Cow<'static, str>> {
    (count > 0).then(|| {
        if count == 1 {
            t!("conversation_transcript.rewind.pending_notice.one")
        } else {
            t!(
                "conversation_transcript.rewind.pending_notice.other",
                count = count
            )
        }
    })
}

/// The workflows a cut Turn launched stop with it, by name.
fn workflows_line(names: &[String]) -> Option<Cow<'static, str>> {
    (!names.is_empty()).then(|| {
        let joined = names.join(", ");
        if names.len() == 1 {
            t!(
                "conversation_transcript.rewind.workflows_notice.one",
                names = joined
            )
        } else {
            t!(
                "conversation_transcript.rewind.workflows_notice.other",
                count = names.len(),
                names = joined
            )
        }
    })
}

/// Plain background processes carry no Turn attribution: the rewind does
/// not stop them, and the dialog says so instead of staying quiet about
/// work that will outlive the cut.
fn processes_line(count: u32) -> Option<Cow<'static, str>> {
    (count > 0).then(|| {
        if count == 1 {
            t!("conversation_transcript.rewind.processes_note.one")
        } else {
            t!(
                "conversation_transcript.rewind.processes_note.other",
                count = count
            )
        }
    })
}

/// A plan refusal the engine phrases stably, mapped to catalog copy so a
/// refusal reads like the rest of the dialog instead of raw RPC prose.
/// The engine phrases are pinned by tests on both sides.
pub(crate) fn rewind_refusal_copy(error: &str) -> Option<Cow<'static, str>> {
    if error.contains("compacted away") {
        Some(t!("conversation_transcript.rewind.refusal.compacted"))
    } else if error.contains("not a Git work tree") {
        Some(t!("conversation_transcript.rewind.refusal.non_git"))
    } else {
        None
    }
}

/// The dialog's title and body, and whether it can be confirmed.
pub(crate) fn rewind_question(plan: &RewindReply) -> (Cow<'static, str>, String, bool) {
    if !plan.conflicts.is_empty() {
        let lines = plan
            .conflicts
            .iter()
            .map(|conflict| {
                let why = match conflict.reason {
                    RewindConflictReason::Changed => {
                        t!("conversation_transcript.rewind.conflict_changed")
                    }
                    RewindConflictReason::NotStored => {
                        t!("conversation_transcript.rewind.conflict_not_stored")
                    }
                    RewindConflictReason::Unsafe => {
                        t!("conversation_transcript.rewind.conflict_unsafe")
                    }
                };
                t!(
                    "conversation_transcript.rewind.conflict_line",
                    path = conflict.path.as_str(),
                    reason = why.as_ref()
                )
                .into_owned()
            })
            .collect();
        return (
            t!("conversation_transcript.rewind.title_cant"),
            format!(
                "{}\n{}",
                t!("conversation_transcript.rewind.conflicts_intro"),
                listed(lines)
            ),
            false,
        );
    }
    let mut body = removed_intro(plan.turns).into_owned();
    if let Some(steps) = steps_line(plan.steps) {
        body.push('\n');
        body.push_str(steps.as_ref());
    }
    if let Some(line) = stop_line(plan.will_stop_turn) {
        body.push('\n');
        body.push_str(line.as_ref());
    }
    if let Some(line) = pending_line(plan.pending_count) {
        body.push('\n');
        body.push_str(line.as_ref());
    }
    if let Some(line) = workflows_line(&plan.workflows_to_stop) {
        body.push('\n');
        body.push_str(line.as_ref());
    }
    if let Some(line) = processes_line(plan.running_processes) {
        body.push('\n');
        body.push_str(line.as_ref());
    }
    if plan.files.is_empty() {
        return (
            t!("conversation_transcript.rewind.title_confirm"),
            body,
            true,
        );
    }
    let lines = plan
        .files
        .iter()
        .map(|file| match file.action {
            RewindAction::Restore => t!(
                "conversation_transcript.rewind.file_restored",
                path = file.path.as_str()
            )
            .into_owned(),
            RewindAction::Delete => t!(
                "conversation_transcript.rewind.file_deleted",
                path = file.path.as_str()
            )
            .into_owned(),
        })
        .collect();
    (
        t!("conversation_transcript.rewind.title_confirm"),
        format!(
            "{}\n{}\n{}",
            body,
            t!("conversation_transcript.rewind.files_intro"),
            listed(lines)
        ),
        true,
    )
}

/// One-line outcome for the notice after an applied rewind.
pub(crate) fn rewind_summary(reply: &RewindReply) -> String {
    match reply.files.len() {
        0 => t!("conversation_transcript.rewind.summary.none").into_owned(),
        1 => t!("conversation_transcript.rewind.summary.one", count = 1).into_owned(),
        n => t!("conversation_transcript.rewind.summary.other", count = n).into_owned(),
    }
}

/// The confirm modal. Without `confirm` it only offers to close.
pub(crate) fn rewind_dialog(
    theme: &Theme,
    viewport: gpui::Size<gpui::Pixels>,
    title: impl Into<SharedString>,
    question: String,
    cancel_out: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    cancel: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    confirm: Option<impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static>,
) -> AnyElement {
    let title = title.into();
    let mut buttons = div()
        .mt(px(16.0))
        .flex()
        .flex_row()
        .justify_end()
        .gap(px(8.0));
    match confirm {
        Some(confirm) => {
            buttons = buttons
                .child(
                    crate::popover::btn_ghost(
                        theme,
                        t!("conversation_transcript.common.cancel").as_ref(),
                        "rewind-cancel",
                    )
                    .id("rewind-cancel")
                    .debug_selector(|| "rewind-cancel".to_string())
                    .on_click(cancel),
                )
                .child(
                    crate::popover::btn_primary(
                        theme,
                        t!("conversation_transcript.rewind.button").as_ref(),
                    )
                    .id("rewind-confirm")
                    .debug_selector(|| "rewind-confirm".to_string())
                    .on_click(confirm),
                );
        }
        None => {
            buttons = buttons.child(
                crate::popover::btn_ghost(
                    theme,
                    t!("conversation_transcript.common.close").as_ref(),
                    "rewind-cancel",
                )
                .id("rewind-cancel")
                .debug_selector(|| "rewind-cancel".to_string())
                .on_click(cancel),
            );
        }
    }
    let card = crate::popover::dialog_card(theme)
        .on_mouse_down_out(cancel_out)
        .child(crate::popover::dialog_title(theme, title.as_str()))
        .child(
            div()
                .mt(px(6.0))
                .child(crate::popover::dialog_body(theme, question)),
        )
        .child(buttons)
        .into_any_element();
    crate::popover::modal("rewind-dialog", viewport, card)
}

/// A user row's rewind affordance, drawn inside the hover metadata strip:
/// the strip's own reveal is the hover gate, so the button itself stays
/// plain (same square/hover treatment as the strip's edit button).
pub(crate) fn rewind_button(
    id: SharedString,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let hint: SharedString = t!("conversation_transcript.rewind.tooltip").into();
    let selector = id.clone();
    div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .size(px(20.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Theme::CONTROL_RADIUS))
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::ink(0.08)))
        .tooltip(move |_, cx| cx.new(|_| crate::popover::TextTooltip(hint.clone())).into())
        .on_click(on_click)
        .child(
            crate::icons::icon(crate::icons::UNDO_LEFT_ROUND)
                .size(px(14.0))
                .text_color(theme.text_muted),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::{RewindConflict, RewindFile};

    fn plan(files: Vec<RewindFile>, conflicts: Vec<RewindConflict>) -> RewindReply {
        RewindReply {
            turns: 1,
            steps: 0,
            will_stop_turn: false,
            pending_count: 0,
            workflows_to_stop: Vec::new(),
            running_processes: 0,
            files,
            conflicts,
            applied: false,
        }
    }

    #[test]
    fn the_dialog_sizes_the_cut_from_the_reply() {
        let _locale = crate::i18n::tests::locale_lock();
        let (title, body, can) = rewind_question(&RewindReply {
            turns: 3,
            steps: 2,
            will_stop_turn: true,
            pending_count: 2,
            workflows_to_stop: vec!["audit".into()],
            running_processes: 1,
            files: Vec::new(),
            conflicts: Vec::new(),
            applied: false,
        });
        assert_eq!(title, t!("conversation_transcript.rewind.title_confirm"));
        assert!(can);
        assert!(
            body.contains(
                t!(
                    "conversation_transcript.rewind.removed_intro.other",
                    count = 3
                )
                .as_ref()
            ),
            "{body}"
        );
        assert!(
            body.contains(
                t!(
                    "conversation_transcript.rewind.steps_undone.other",
                    count = 2
                )
                .as_ref()
            ),
            "{body}"
        );
        // The confirm's side effects each get their own line: the stop,
        // the sweep, the workflows, and the processes left running.
        assert!(
            body.contains(t!("conversation_transcript.rewind.stop_notice").as_ref()),
            "{body}"
        );
        assert!(
            body.contains(
                t!(
                    "conversation_transcript.rewind.pending_notice.other",
                    count = 2
                )
                .as_ref()
            ),
            "{body}"
        );
        assert!(
            body.contains(
                t!(
                    "conversation_transcript.rewind.workflows_notice.one",
                    names = "audit"
                )
                .as_ref()
            ),
            "{body}"
        );
        assert!(
            body.contains(t!("conversation_transcript.rewind.processes_note.one").as_ref()),
            "{body}"
        );
        // A single Turn reads as one, and no side effect is not a line.
        let (_, body, _) = rewind_question(&RewindReply {
            turns: 1,
            steps: 0,
            ..plan(Vec::new(), Vec::new())
        });
        assert!(
            body.contains(t!("conversation_transcript.rewind.removed_intro.one").as_ref()),
            "{body}"
        );
        assert!(!body.contains("steps"));
        assert!(!body.contains(t!("conversation_transcript.rewind.stop_notice").as_ref()));
    }

    /// The two stable engine refusals read as catalog copy; anything else
    /// falls back to the raw message.
    #[test]
    fn the_stable_refusals_map_to_catalog_copy() {
        let _locale = crate::i18n::tests::locale_lock();
        let compacted = "that message was compacted away — the summary no longer holds it verbatim";
        assert_eq!(
            rewind_refusal_copy(compacted).as_deref(),
            Some(t!("conversation_transcript.rewind.refusal.compacted").as_ref())
        );
        let non_git = "the chat's working directory is not a Git work tree";
        assert_eq!(
            rewind_refusal_copy(non_git).as_deref(),
            Some(t!("conversation_transcript.rewind.refusal.non_git").as_ref())
        );
        assert_eq!(rewind_refusal_copy("something else entirely"), None);
    }

    #[test]
    fn a_clean_plan_lists_its_files_and_can_be_confirmed() {
        let _locale = crate::i18n::tests::locale_lock();
        let (title, body, can) = rewind_question(&plan(
            vec![
                RewindFile {
                    path: "a.txt".into(),
                    action: RewindAction::Restore,
                },
                RewindFile {
                    path: "b.txt".into(),
                    action: RewindAction::Delete,
                },
            ],
            Vec::new(),
        ));
        assert_eq!(title, t!("conversation_transcript.rewind.title_confirm"));
        assert!(can);
        assert!(
            body.contains(
                t!(
                    "conversation_transcript.rewind.file_restored",
                    path = "a.txt"
                )
                .as_ref()
            ) && body.contains(
                t!(
                    "conversation_transcript.rewind.file_deleted",
                    path = "b.txt"
                )
                .as_ref()
            )
        );
    }

    #[test]
    fn a_conflict_only_explains_and_cannot_be_confirmed() {
        let _locale = crate::i18n::tests::locale_lock();
        let (title, body, can) = rewind_question(&plan(
            Vec::new(),
            vec![RewindConflict {
                path: "a.txt".into(),
                reason: RewindConflictReason::Changed,
            }],
        ));
        assert_eq!(title, t!("conversation_transcript.rewind.title_cant"));
        assert!(!can);
        assert!(
            body.contains(
                t!(
                    "conversation_transcript.rewind.conflict_line",
                    path = "a.txt",
                    reason = t!("conversation_transcript.rewind.conflict_changed").as_ref()
                )
                .as_ref()
            )
        );
    }

    #[test]
    fn a_long_plan_counts_what_it_does_not_list() {
        let _locale = crate::i18n::tests::locale_lock();
        let files = (0..10)
            .map(|n| RewindFile {
                path: format!("f{n}"),
                action: RewindAction::Restore,
            })
            .collect();
        let (_, body, _) = rewind_question(&plan(files, Vec::new()));
        assert!(
            body.ends_with(t!("conversation_transcript.rewind.and_more", count = 2).as_ref()),
            "{body}"
        );
        assert!(!body.contains("f8"));
    }
}
