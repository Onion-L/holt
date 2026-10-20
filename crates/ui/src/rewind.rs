//! Rewind to a tool step (ADR-rewind-to-a-step-is-all-or-nothing): a chip's
//! rewind affordance asks the engine for the plan (`RewindToStep` dry run),
//! confirms it in a dialog over the conversation, then applies it. A plan
//! with conflicts cannot be confirmed — the dialog only says why.

use gpui::{
    AnyElement, AppContext as _, InteractiveElement as _, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, div, px,
};
use holt_proto::{RewindAction, RewindConflictReason, RewindReply};

use crate::theme::Theme;

/// The step a pending confirm dialog would rewind to, with its plan.
#[derive(Clone)]
pub(crate) struct RewindConfirm {
    pub chat_id: String,
    pub tool_call_id: String,
    pub plan: RewindReply,
}

/// Run `RewindToStep`; `Err` carries a phrased failure.
pub(crate) async fn rewind_call(
    engine: crate::state::EngineHandle,
    chat_id: String,
    tool_call_id: String,
    dry_run: bool,
) -> Result<RewindReply, String> {
    let params = serde_json::json!({
        "chatId": chat_id,
        "toolCallId": tool_call_id,
        "dryRun": dry_run,
    });
    let value = engine
        .client()
        .call(holt_rpc::methods::REWIND_TO_STEP, params)
        .await
        .map_err(|error| format!("Rewind failed: {error}"))?;
    serde_json::from_value(value).map_err(|_| "Rewind failed: malformed reply".to_string())
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
        text.push_str(&format!("\n…and {more} more"));
    }
    text
}

/// The dialog's title and body, and whether it can be confirmed.
pub(crate) fn rewind_question(plan: &RewindReply) -> (&'static str, String, bool) {
    if !plan.conflicts.is_empty() {
        let lines = plan
            .conflicts
            .iter()
            .map(|conflict| {
                let why = match conflict.reason {
                    RewindConflictReason::Changed => "changed since that step",
                    RewindConflictReason::NotStored => "content was not saved",
                    RewindConflictReason::Unsafe => "outside the working tree",
                };
                format!("{} — {why}", conflict.path)
            })
            .collect();
        return (
            "Can't rewind",
            format!(
                "Nothing was changed. These files block it:\n{}",
                listed(lines)
            ),
            false,
        );
    }
    let conversation = "The conversation after this step will be removed.";
    if plan.files.is_empty() {
        return ("Rewind to this step?", conversation.to_string(), true);
    }
    let lines = plan
        .files
        .iter()
        .map(|file| match file.action {
            RewindAction::Restore => format!("{} — restored", file.path),
            RewindAction::Delete => format!("{} — deleted", file.path),
        })
        .collect();
    (
        "Rewind to this step?",
        format!(
            "{conversation} These files go back to how they were:\n{}",
            listed(lines)
        ),
        true,
    )
}

/// One-line outcome for the notice after an applied rewind.
pub(crate) fn rewind_summary(reply: &RewindReply) -> String {
    match reply.files.len() {
        0 => "Rewound the conversation.".into(),
        1 => "Rewound the conversation and 1 file.".into(),
        n => format!("Rewound the conversation and {n} files."),
    }
}

/// The confirm modal. Without `confirm` it only offers to close.
pub(crate) fn rewind_dialog(
    theme: &Theme,
    viewport: gpui::Size<gpui::Pixels>,
    title: &str,
    question: String,
    cancel_out: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    cancel: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    confirm: Option<impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static>,
) -> AnyElement {
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
                    crate::popover::btn_ghost(theme, "Cancel", "rewind-cancel")
                        .id("rewind-cancel")
                        .debug_selector(|| "rewind-cancel".to_string())
                        .on_click(cancel),
                )
                .child(
                    crate::popover::btn_primary(theme, "Rewind")
                        .id("rewind-confirm")
                        .debug_selector(|| "rewind-confirm".to_string())
                        .on_click(confirm),
                );
        }
        None => {
            buttons = buttons.child(
                crate::popover::btn_ghost(theme, "Close", "rewind-cancel")
                    .id("rewind-cancel")
                    .debug_selector(|| "rewind-cancel".to_string())
                    .on_click(cancel),
            );
        }
    }
    let card = crate::popover::dialog_card(theme)
        .on_mouse_down_out(cancel_out)
        .child(crate::popover::dialog_title(theme, title))
        .child(
            div()
                .mt(px(6.0))
                .child(crate::popover::dialog_body(theme, question)),
        )
        .child(buttons)
        .into_any_element();
    crate::popover::modal("rewind-dialog", viewport, card)
}

/// A chip's rewind affordance: faint until its chip is hovered.
pub(crate) fn rewind_button(
    group: SharedString,
    id: SharedString,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let hint: SharedString = "Rewind to this step".into();
    div()
        .id(id)
        .flex_none()
        .size(px(20.0))
        .mr(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .cursor_pointer()
        .opacity(0.0)
        .group_hover(group, |s| s.opacity(1.0))
        .hover(|s| s.bg(crate::theme::ink(0.06)))
        .tooltip(move |_, cx| cx.new(|_| crate::popover::TextTooltip(hint.clone())).into())
        .on_click(on_click)
        .child(
            crate::icons::icon(crate::icons::RESTART)
                .size(px(12.0))
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
            files,
            conflicts,
            applied: false,
        }
    }

    #[test]
    fn a_clean_plan_lists_its_files_and_can_be_confirmed() {
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
        assert_eq!(title, "Rewind to this step?");
        assert!(can);
        assert!(body.contains("a.txt — restored") && body.contains("b.txt — deleted"));
    }

    #[test]
    fn a_conflict_only_explains_and_cannot_be_confirmed() {
        let (title, body, can) = rewind_question(&plan(
            Vec::new(),
            vec![RewindConflict {
                path: "a.txt".into(),
                reason: RewindConflictReason::Changed,
            }],
        ));
        assert_eq!(title, "Can't rewind");
        assert!(!can);
        assert!(body.contains("a.txt — changed since that step"));
    }

    #[test]
    fn a_long_plan_counts_what_it_does_not_list() {
        let files = (0..10)
            .map(|n| RewindFile {
                path: format!("f{n}"),
                action: RewindAction::Restore,
            })
            .collect();
        let (_, body, _) = rewind_question(&plan(files, Vec::new()));
        assert!(body.ends_with("…and 2 more"), "{body}");
        assert!(!body.contains("f8"));
    }
}
