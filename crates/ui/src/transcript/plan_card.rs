//! The Plan Mode approval card (ADR-0025): the plan DOCUMENT in the
//! transcript flow — the submitted plan as a bounded scrollable block
//! (snapshotted at submission, so it shows what was reviewed). The verdict
//! itself moved to the composer's approval bar (`composer::approval_bar`,
//! the same surface the ADR-0014 gate uses): a pending card renders the
//! document only, settled cards render their marker. The document path
//! stays in the part's data — the card does not show it.
//!
//! [`pending_plan_approval`] is the bar's pending scan;
//! [`resolve_plan_approval`] its verdict channel.

use gpui::{AnyElement, App, Entity, SharedString, div, prelude::*, px};

use holt_doc::{MessagePart, PlanApprovalState, PlanApprovalVerdict, SessionMessageEntry};
use holt_rpc::methods;

use super::Transcript;
use super::approval::{VerdictTint, verdict_tint_color};
use crate::state::AppState;
use crate::theme::Theme;

/// The latest still-pending plan approval in a transcript, keyed
/// `{entry}#{part}` like its card row — the composer approval bar's
/// ADR-0025 producer (one plan awaits a verdict at a time, and the engine
/// resolves whatever is pending in the chat).
pub fn pending_plan_approval(transcript: &[SessionMessageEntry]) -> Option<String> {
    transcript
        .iter()
        .rev()
        .flat_map(|entry| entry.parts.iter().rev().map(move |part| (entry, part)))
        .find_map(|(entry, part)| match part {
            MessagePart::PlanApproval {
                id,
                state: PlanApprovalState::Pending,
                ..
            } => Some(format!("{}#{}", entry.id, id)),
            _ => None,
        })
}

/// Send the plan verdict (fire-and-forget: failures warn; the doc's
/// settled card is what settles the UI). Params `{chatId, verdict,
/// feedback?}` per the engine contract: approve exits Plan Mode and
/// restores the entry permission mode, reject keeps planning with a
/// non-empty feedback enqueued as the revision's next planning input,
/// remain returns to drafting.
pub fn resolve_plan_approval(
    state: &Entity<AppState>,
    verdict: &'static str,
    feedback: Option<String>,
    cx: &mut App,
) {
    let (engine, chat_id) = {
        let state = state.read(cx);
        (state.engine().cloned(), state.selected_chat.clone())
    };
    let (Some(engine), Some(chat_id)) = (engine, chat_id) else {
        return;
    };
    let mut params = serde_json::json!({ "chatId": chat_id, "verdict": verdict });
    if let Some(feedback) = feedback {
        params["feedback"] = serde_json::Value::String(feedback);
    }
    cx.spawn(async move |_| {
        if let Err(err) = engine
            .client()
            .call(methods::RESOLVE_PLAN_APPROVAL, params)
            .await
        {
            tracing::warn!(error = %err, "ResolvePlanApproval failed");
        }
    })
    .detach();
}

/// The submitted plan's document, rendered inside the approval card: a
/// bounded, scrollable mono block under the header — max width for
/// readable line lengths, max height so a long plan never stretches the
/// transcript. `.occlude()` keeps one wheel gesture from scrolling both
/// this block and the outer transcript list (ADR-0013).
fn render_plan_content(text: &str, row_id: &SharedString, theme: &Theme) -> gpui::AnyElement {
    div()
        .id(SharedString::from(format!("plan-content-{row_id}")))
        .debug_selector(move || format!("plan-content-{row_id}"))
        .mt(px(8.0))
        .max_w(px(720.0))
        .max_h(px(320.0))
        .w_full()
        .overflow_y_scroll()
        .occlude()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.hairline(0.12))
        .bg(theme.ink(0.04))
        .px(px(12.0))
        .py(px(10.0))
        .font_family(theme.font_mono.clone())
        .text_size(crate::typography::ui_rems(11.5))
        .line_height(px(17.0))
        .text_color(theme.text_muted)
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

impl Transcript {
    /// The Plan Mode approval strip (ADR-0025): the same flat transcript
    /// language as the permission gate — hairlines, a mono lead, and the
    /// verdict affordances bottom-right. Three user actions: Approve,
    /// Reject (with feedback), and Stay in planning. Settled cards render
    /// their verdict marker only.
    pub(super) fn render_plan_approval_card(
        &mut self,
        row_id: &SharedString,
        content: &SharedString,
        state: &PlanApprovalState,
        theme: &Theme,
    ) -> AnyElement {
        let settled = |text: String, tint: VerdictTint| {
            div()
                .py(px(4.0))
                .w_full()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .py(px(10.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(verdict_tint_color(tint, theme))
                                .child(SharedString::from(text)),
                        ),
                )
                .into_any_element()
        };
        let verdict = match state {
            PlanApprovalState::Settled { verdict } => verdict,
            PlanApprovalState::Pending => {
                // Pending: the document only — the verdict lives in the
                // composer's approval bar.
                return div()
                    .py(px(4.0))
                    .w_full()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .border_t_1()
                            .border_b_1()
                            .border_color(theme.border)
                            .py(px(10.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .child(div().w_full().text_color(theme.text).child("Proposed plan"))
                            .child(render_plan_content(content, row_id, theme)),
                    )
                    .into_any_element();
            }
        };
        let (text, tint) = match verdict {
            PlanApprovalVerdict::Approved => ("✓ Plan approved".to_string(), VerdictTint::Neutral),
            PlanApprovalVerdict::Rejected => ("⊘ Plan rejected".to_string(), VerdictTint::Danger),
            PlanApprovalVerdict::Remained => {
                ("⏸ Still in planning".to_string(), VerdictTint::Neutral)
            }
            PlanApprovalVerdict::Dismissed => {
                ("— Planning dismissed".to_string(), VerdictTint::Neutral)
            }
        };
        settled(text, tint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::model::RowKind;
    use holt_doc::{MessagePart, MessageRole, SessionMessageEntry};

    fn entry(id: &str, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn plan_part(id: &str, state: PlanApprovalState) -> MessagePart {
        plan_part_with_content(id, "# The plan\n- step one", state)
    }

    fn plan_part_with_content(id: &str, content: &str, state: PlanApprovalState) -> MessagePart {
        MessagePart::PlanApproval {
            id: id.into(),
            content: content.to_string(),
            state,
        }
    }

    fn system_entry(id: &str, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            role: holt_doc::MessageRole::System,
            ..entry(id, parts)
        }
    }

    /// Row model: a plan submission splices its own card row, and the
    /// verdict settles it in place — the card never disappears from the
    /// transcript, it renders its marker instead.
    #[test]
    fn plan_approval_rows_splice_and_settle_in_place() {
        use crate::markdown::parser::{BlockTree, parse_full};
        use std::sync::Arc;

        let mut parse = |_: &str, text: &str| Arc::new(parse_full(text)) as Arc<BlockTree>;
        let rows = crate::transcript::rows_for_entry(
            &system_entry("s1", vec![plan_part("p1", PlanApprovalState::Pending)]),
            false,
            &mut parse,
        );
        assert_eq!(rows.len(), 1);
        let RowKind::PlanApproval { content, state, .. } = &rows[0].kind else {
            panic!("expected the plan approval row");
        };
        assert_eq!(content.as_ref(), "# The plan\n- step one");
        assert!(matches!(state, PlanApprovalState::Pending));

        let rows = crate::transcript::rows_for_entry(
            &system_entry(
                "s1",
                vec![plan_part(
                    "p1",
                    PlanApprovalState::Settled {
                        verdict: PlanApprovalVerdict::Approved,
                    },
                )],
            ),
            false,
            &mut parse,
        );
        let RowKind::PlanApproval { state, .. } = &rows[0].kind else {
            panic!("expected the settled plan approval row");
        };
        assert!(matches!(
            state,
            PlanApprovalState::Settled {
                verdict: PlanApprovalVerdict::Approved
            }
        ));
    }

    #[test]
    fn pending_plan_approval_scans_from_the_tail_keyed_like_the_row() {
        let pending = plan_part("p1", PlanApprovalState::Pending);
        let settled = plan_part(
            "p2",
            PlanApprovalState::Settled {
                verdict: PlanApprovalVerdict::Approved,
            },
        );
        let t = vec![
            system_entry("s1", vec![pending.clone()]),
            system_entry("s2", vec![settled]),
        ];
        assert_eq!(pending_plan_approval(&t), Some("s1#p1".to_string()));
        // A settled card never reports; the latest pending wins.
        let t = vec![
            system_entry("s1", vec![pending.clone()]),
            system_entry("s2", vec![plan_part("p9", PlanApprovalState::Pending)]),
        ];
        assert_eq!(pending_plan_approval(&t), Some("s2#p9".to_string()));
        assert_eq!(pending_plan_approval(&[]), None);
    }

    /// Entity + render test: the pending card draws the plan document
    /// with NO affordances (the verdict lives in the composer's approval
    /// bar); the settled card draws its verdict marker.
    #[gpui::test]
    fn plan_card_pending_document_and_settled_marker(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let (transcript, cx) = cx.add_window_view(|_, cx| Transcript::new(state.clone(), cx));

        transcript.update(cx, |this, cx| {
            this.chat_id = Some("chat-1".into());
            cx.notify();
        });
        state.update(cx, |s, cx| {
            s.transcript.push(system_entry(
                "s1",
                vec![plan_part("p1", PlanApprovalState::Pending)],
            ));
            cx.notify();
        });
        cx.run_until_parked();
        transcript.update(cx, |this, _| {
            assert!(
                this.rows
                    .iter()
                    .any(|row| matches!(row.kind, RowKind::PlanApproval { .. }))
            );
        });
        // The document block draws; nothing answerable remains in the
        // transcript (no buttons, no feedback editor).
        let content = cx
            .debug_bounds("plan-content-s1#p1")
            .expect("plan content block missing");
        assert!(content.size.height <= gpui::px(321.0));
        assert!(content.size.width <= gpui::px(721.0));
        assert!(cx.debug_bounds("plan-approve-s1#p1").is_none());

        // The settle swaps the document for the verdict marker.
        state.update(cx, |s, cx| {
            s.transcript[0] = system_entry(
                "s1",
                vec![plan_part(
                    "p1",
                    PlanApprovalState::Settled {
                        verdict: PlanApprovalVerdict::Approved,
                    },
                )],
            );
            cx.notify();
        });
        cx.run_until_parked();
        cx.run_until_parked();
        transcript.update(cx, |this, _| {
            assert!(!this.rows.iter().any(|row| {
                matches!(
                    &row.kind,
                    RowKind::PlanApproval {
                        state: PlanApprovalState::Pending,
                        ..
                    }
                )
            }));
        });
        assert!(cx.debug_bounds("plan-content-s1#p1").is_none());
    }
}
