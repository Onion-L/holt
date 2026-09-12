//! The Plan Mode approval card (ADR-0025): a standalone transcript
//! component for plan submissions — it borrows the permission gate strip's
//! visual language (hairlines, mono lead, bottom-right affordances) but is
//! its own component with its own editor state, not a reuse of the
//! ADR-0014 card. The card leads with the document pointer, renders the
//! submitted plan as a bounded scrollable block (snapshotted at
//! submission, so it shows what was reviewed), and offers the three
//! verdict affordances; settled cards render their marker only. The
//! document path stays in the part's data — the card does not show it.
//!
//! Interactive state (the feedback editor) lives on the `Transcript`
//! entity keyed by plan id — never in `RowKind`, so a row re-splice can't
//! drop a half-written note.

use gpui::{
    AnyElement, Context, Entity, Focusable as _, KeyDownEvent, SharedString, Window, div,
    prelude::*, px,
};

use holt_doc::{PlanApprovalState, PlanApprovalVerdict};
use holt_rpc::methods;

use super::approval::{VerdictTint, verdict_tint_color};
use super::{ApprovalNote, Transcript};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::theme::Theme;

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
        plan_id: &SharedString,
        content: &Option<SharedString>,
        state: &PlanApprovalState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor_key = plan_id.to_string();
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
                let feedback_input = self
                    .plan_notes
                    .get(&editor_key)
                    .map(|note| note.input.clone());
                let id_approve = plan_id.clone();
                let id_reject = plan_id.clone();
                let id_remain = plan_id.clone();
                // Approve — the primary action (subtle-raised idiom).
                let approve = div()
                    .id(SharedString::from(format!("plan-approve-{plan_id}")))
                    .debug_selector(move || format!("plan-approve-{plan_id}"))
                    .flex_none()
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.hairline(0.14))
                    .bg(theme.ink(0.09))
                    .text_size(crate::typography::ui_rems(11.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|el| el.bg(theme.ink(0.14)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.resolve_plan_verdict(&id_approve, "approve", None, cx);
                    }))
                    .child("Approve");
                // Reject — restrained danger; opens the feedback editor.
                let reject = div()
                    .id(SharedString::from(format!("plan-reject-{plan_id}")))
                    .debug_selector(move || format!("plan-reject-{plan_id}"))
                    .flex_none()
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.danger.opacity(0.3))
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.danger_muted)
                    .cursor_pointer()
                    .hover(|el| el.bg(theme.danger.opacity(0.06)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_plan_feedback(id_reject.to_string(), window, cx);
                    }))
                    .child(if feedback_input.is_some() {
                        "Hide"
                    } else {
                        "Reject…"
                    });
                // Stay in planning — neutral ghost.
                let remain = div()
                    .id(SharedString::from(format!("plan-remain-{plan_id}")))
                    .debug_selector(move || format!("plan-remain-{plan_id}"))
                    .flex_none()
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.hairline(0.14))
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|el| el.bg(theme.ink(0.05)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.resolve_plan_verdict(&id_remain, "remain", None, cx);
                    }))
                    .child("Stay in planning");
                let card_shape = div()
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
                                    .w_full()
                                    .text_color(theme.text)
                                    .child("Plan submitted for approval"),
                            )
                            .when_some(content.as_ref(), |card, text| {
                                card.child(render_plan_content(text, row_id, theme))
                            })
                            .child(
                                div()
                                    .mt(px(4.0))
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .line_height(px(15.0))
                                    .text_color(theme.text_faint)
                                    .child(
                                        "Approve to start implementation · reject with feedback to revise · stay to keep planning",
                                    ),
                            )
                            .child(
                                div()
                                    .mt(px(8.0))
                                    .flex()
                                    .flex_row()
                                    .justify_end()
                                    .flex_wrap()
                                    .gap(px(6.0))
                                    .child(approve)
                                    .child(reject)
                                    .child(remain),
                            ),
                    )
                    .when_some(feedback_input, |card, input| {
                        card.child(self.render_plan_feedback_editor(
                            row_id,
                            &editor_key,
                            plan_id,
                            input,
                            theme,
                            cx,
                        ))
                    })
                    .into_any_element();
                return card_shape;
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

    /// The rejection feedback editor at the strip's foot: the feedback is
    /// enqueued as the revision loop's next planning input. Enter submits
    /// (= Reject with feedback); a blank note is a plain reject.
    fn render_plan_feedback_editor(
        &mut self,
        row_id: &SharedString,
        editor_key: &str,
        plan_id: &SharedString,
        input: Entity<ComposerInput>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id_for_key = editor_key.to_string();
        let id_for_submit = plan_id.to_string();
        div()
            .id(SharedString::from(format!(
                "plan-feedback-{editor_key}-{row_id}"
            )))
            .w_full()
            .mt(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.plan_notes.remove(&id_for_key);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(
                div()
                    .w_full()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.hairline(0.12))
                    .bg(theme.ink(0.04))
                    .px(px(10.0))
                    .py(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .child(input),
            )
            .child(
                div().flex().flex_row().justify_end().child(
                    div()
                        .id(SharedString::from(format!(
                            "plan-reject-feedback-{editor_key}-{row_id}"
                        )))
                        .flex_none()
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.danger.opacity(0.3))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger_muted)
                        .cursor_pointer()
                        .hover(|el| el.bg(theme.danger.opacity(0.06)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.submit_plan_feedback(&id_for_submit, cx);
                        }))
                        .child("Reject with feedback"),
                ),
            )
            .into_any_element()
    }

    /// Open (or close) the rejection feedback editor for one plan. The
    /// editor entity lives on the Transcript keyed by the plan id — a row
    /// re-splice can't drop a half-written note.
    fn toggle_plan_feedback(
        &mut self,
        plan_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor_key = plan_id.to_string();
        if self.plan_notes.remove(&editor_key).is_some() {
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            ComposerInput::new(
                "What to change — enqueued as the revision's next planning input",
                cx,
            )
        });
        let id = plan_id.clone();
        let events = cx.subscribe(&input, move |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.submit_plan_feedback(&id, cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let handle = input.read(cx).focus_handle(cx);
        self.plan_notes.insert(
            editor_key,
            ApprovalNote {
                input,
                _events: events,
            },
        );
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Enter in the feedback editor = Reject with feedback; a blank note is
    /// a plain reject.
    fn submit_plan_feedback(&mut self, plan_id: &str, cx: &mut Context<Self>) {
        let editor_key = plan_id.to_string();
        let feedback = self
            .plan_notes
            .get(&editor_key)
            .map(|note| note.input.read(cx).text().trim().to_string())
            .filter(|note| !note.is_empty());
        self.resolve_plan_verdict(plan_id, "reject", feedback, cx);
    }

    /// Send the plan verdict (fire-and-forget: failures warn; the doc's
    /// settled card is what settles the strip). The feedback editor, if
    /// open, closes with the verdict. Approve and stay carry no feedback.
    fn resolve_plan_verdict(
        &mut self,
        plan_id: &str,
        verdict: &'static str,
        feedback: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.plan_notes.remove(&format!("plan-{plan_id}"));
        cx.notify();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut params = serde_json::json!({ "planId": plan_id, "verdict": verdict });
        if let Some(feedback) = feedback {
            params["feedback"] = serde_json::Value::String(feedback);
        }
        cx.spawn(async move |_, _| {
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
    /// Drop plan feedback editors whose card no longer shows a pending
    /// submission (verdict landed, chat switched, transcript switched docs)
    /// — called once per render.
    pub(super) fn prune_plan_notes(&mut self) {
        if self.plan_notes.is_empty() {
            return;
        }
        let pending: Vec<String> = self
            .rows
            .iter()
            .filter_map(|row| match &row.kind {
                super::model::RowKind::PlanApproval {
                    plan_id,
                    state: PlanApprovalState::Pending,
                    ..
                } => Some(plan_id.to_string()),
                _ => None,
            })
            .collect();
        self.plan_notes.retain(|id, _| pending.contains(id));
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
        plan_part_with_content(id, Some("# The plan\n- step one"), state)
    }

    fn plan_part_with_content(
        id: &str,
        content: Option<&str>,
        state: PlanApprovalState,
    ) -> MessagePart {
        MessagePart::PlanApproval {
            id: id.into(),
            plan_id: "plan-1".into(),
            plan_path: "/repo/.holt/plans/chat-1-plan-1.md".into(),
            content: content.map(str::to_string),
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
        let RowKind::PlanApproval {
            plan_id,
            content,
            state,
            ..
        } = &rows[0].kind
        else {
            panic!("expected the plan approval row");
        };
        assert_eq!(plan_id.as_ref(), "plan-1");
        assert_eq!(content.as_deref(), Some("# The plan\n- step one"));
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

    /// Entity + render test: the pending card draws its three affordances
    /// (Approve / Reject… / Stay in planning); the feedback editor opens
    /// and closes keyed by plan id; the settle prunes a stale editor and
    /// the settled card draws its marker.
    #[gpui::test]
    fn plan_card_affordances_feedback_and_render(cx: &mut gpui::TestAppContext) {
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
        transcript.update(cx, |this, _| {
            assert!(
                this.rows
                    .iter()
                    .any(|row| matches!(row.kind, RowKind::PlanApproval { .. }))
            );
        });
        cx.run_until_parked();
        transcript.update(cx, |this, _| {
            assert!(
                this.rows
                    .iter()
                    .any(|row| matches!(row.kind, RowKind::PlanApproval { .. })),
                "rows rebuilt by the watch pump"
            );
        });
        // The pending strip draws all three affordances.
        for (selector, label) in [
            ("plan-approve-plan-1", "Approve"),
            ("plan-reject-plan-1", "Reject"),
            ("plan-remain-plan-1", "Stay in planning"),
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{label} affordance missing"
            );
        }
        // The submitted plan renders inside the card, bounded (the block's
        // height never exceeds the scroll cap, width the max width).
        let content = cx
            .debug_bounds("plan-content-s1#p1")
            .expect("plan content block missing");
        assert!(content.size.height <= gpui::px(321.0));
        assert!(content.size.width <= gpui::px(721.0));

        // Reject… opens the feedback editor keyed by plan id; toggling
        // closes it.
        cx.update(|window, cx| {
            transcript.update(cx, |this, cx| {
                this.toggle_plan_feedback("plan-1".into(), window, cx);
            });
        });
        transcript.update(cx, |this, _| {
            assert!(this.plan_notes.contains_key("plan-1"));
        });
        cx.update(|window, cx| {
            transcript.update(cx, |this, cx| {
                this.toggle_plan_feedback("plan-1".into(), window, cx);
            });
        });
        transcript.update(cx, |this, _| assert!(this.approval_notes.is_empty()));

        // The settle prunes a stale editor and the settled card draws.
        cx.update(|window, cx| {
            transcript.update(cx, |this, cx| {
                this.toggle_plan_feedback("plan-1".into(), window, cx);
            });
        });
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
            assert!(
                !this.rows.iter().any(|row| {
                    matches!(
                        &row.kind,
                        RowKind::PlanApproval {
                            state: PlanApprovalState::Pending,
                            ..
                        }
                    )
                }),
                "rows after settle: {:?}",
                this.rows
                    .iter()
                    .map(|row| row.id.to_string())
                    .collect::<Vec<_>>()
            );
            this.prune_plan_notes();
            assert!(this.plan_notes.is_empty());
        });
        cx.run_until_parked();
        // The settled card draws its verdict marker.
        assert!(cx.debug_bounds("plan-approve-plan-1").is_none());
    }
}
