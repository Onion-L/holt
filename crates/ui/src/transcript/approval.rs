//! The permission Approval surface (ADR-0014). A pending confirm-changes
//! gate has two cooperating views: this flat marker strip in the
//! transcript flow — what is gated, in context — and the composer's
//! approval bar (`composer::approval_bar`), where the verdict is chosen.
//! The strip shows the gated command/path as a bare mono line plus the
//! working directory as faint metadata and points at the bar; settled
//! gates render their verdict as a small marker on the ordinary tool chip
//! ([`verdict_chip`]). The strip speaks in the neutral scheme (user call:
//! no accent, no amber) — `danger` only for denials.
//!
//! The resolve channel itself ([`resolve_approval`]) is shared by every
//! surface that answers a gate.

use gpui::{
    AnyElement, App, Context, Entity, Hsla, KeyDownEvent, SharedString, Window, div, prelude::*, px,
};

use holt_doc::{GateVerdict, MessagePart, SessionMessageEntry, ToolGate, ToolGateState};
use holt_proto::{ApprovalVerdict, ToolCall};
use holt_rpc::methods;

use super::model::{ToolItem, skill_file_display};
use super::{Transcript, tool_chip_content};
use crate::state::AppState;
use crate::theme::Theme;

/// The latest still-pending gate in a transcript AND its tool call — the
/// composer approval bar's data source (the bar renders the gated target,
/// not just the id), and the Esc-interrupt path's (via the gate-only
/// [`pending_approval_gate`]).
pub fn pending_approval_tool(transcript: &[SessionMessageEntry]) -> Option<(ToolCall, ToolGate)> {
    transcript
        .iter()
        .rev()
        .flat_map(|entry| entry.parts.iter().rev())
        .find_map(|part| match part {
            MessagePart::Tool {
                call,
                gate: Some(gate),
                ..
            } if gate.state == ToolGateState::Pending => Some((call.clone(), gate.clone())),
            _ => None,
        })
}

/// The latest still-pending gate in a transcript, if any.
pub fn pending_approval_gate(transcript: &[SessionMessageEntry]) -> Option<ToolGate> {
    pending_approval_tool(transcript).map(|(_, gate)| gate)
}

/// Send the verdict (fire-and-forget: failures are no-ops engine-side,
/// and the doc's settled gate is what settles the UI). Shared by the
/// transcript strip and the composer approval bar.
pub fn resolve_approval(
    state: &Entity<AppState>,
    approval_id: String,
    verdict: ApprovalVerdict,
    cx: &mut App,
) {
    let Some(engine) = state.read(cx).engine().cloned() else {
        return;
    };
    cx.spawn(async move |_| {
        let params = serde_json::json!({
            "approvalId": approval_id,
            "verdict": verdict,
        });
        if let Err(err) = engine
            .client()
            .call(methods::RESOLVE_APPROVAL, params)
            .await
        {
            tracing::warn!(error = %err, "ResolveApproval failed");
        }
    })
    .detach();
}

/// The strip/bar's shared metadata line: the working directory (when the
/// chat row has one) plus the confirm-changes contract.
pub fn approval_meta(cwd: Option<&str>) -> String {
    match cwd {
        Some(cwd) => format!(
            "cwd: {} · Confirm changes: commands run only after you approve",
            skill_file_display(cwd)
        ),
        None => "Confirm changes: commands run only after you approve".to_string(),
    }
}

/// The bar's terse working-directory line (the strip carries the full
/// confirm-changes contract; the bar's header already says it).
pub fn approval_cwd_line(cwd: &str) -> String {
    format!("cwd: {}", skill_file_display(cwd))
}

/// The gated target rendered as the strip's mono lead line. Bash commands
/// get the shell's `$ ` prefix.
pub fn approval_target(call: &ToolCall) -> String {
    match call {
        ToolCall::Exec { command } => format!("$ {command}"),
        ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => path.clone(),
        ToolCall::ApplyPatch { path } => path.clone().unwrap_or_else(|| "workspace".into()),
        _ => tool_chip_content(call).1,
    }
}

/// Marker color language for a settled verdict: neutral for every pass or
/// automatic exemption (a completed tool chip speaks in muted tones too),
/// `danger` for every form of rejection — denial is the chip's error case,
/// consistent with failed-tool chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictTint {
    Neutral,
    Danger,
}

/// The settled verdict's compact marker text + tint (prototype 3-A's chip
/// suffixes). A denial/rejection carries the note/reason when there was one
/// — that text is the reason the model received, so the transcript shows it.
pub fn verdict_chip(verdict: &GateVerdict) -> (String, VerdictTint) {
    match verdict {
        GateVerdict::Allowed => ("✓ Approved".to_string(), VerdictTint::Neutral),
        GateVerdict::AlwaysAllowed => ("✓ Always allowed".to_string(), VerdictTint::Neutral),
        GateVerdict::Exempted => (
            "⚡ Prefix exempt · auto-passed".to_string(),
            VerdictTint::Neutral,
        ),
        GateVerdict::ReviewPassed => ("👁 Auto-review · passed".to_string(), VerdictTint::Neutral),
        GateVerdict::ReviewRejected { reason } => {
            let text = match reason {
                Some(reason) => format!("👁 Auto-review · rejected · \"{reason}\""),
                None => "👁 Auto-review · rejected".to_string(),
            };
            (text, VerdictTint::Danger)
        }
        GateVerdict::Denied { note } => {
            let text = match note {
                Some(note) => format!("⊘ Denied · \"{note}\""),
                None => "⊘ Denied".to_string(),
            };
            (text, VerdictTint::Danger)
        }
        GateVerdict::Aborted => ("⊘ Interrupted".to_string(), VerdictTint::Danger),
    }
}

pub fn verdict_tint_color(tint: VerdictTint, theme: &Theme) -> Hsla {
    match tint {
        // The neutral tone of an ordinary completed chip's label.
        VerdictTint::Neutral => theme.text_muted,
        VerdictTint::Danger => theme.danger,
    }
}

impl Transcript {
    /// The pending-approval strip: flat transcript content delimited by a
    /// top and bottom hairline (flat-by-default) — never a card, a tinted
    /// banner, or a shadow behind translucency.
    pub(super) fn render_approval_card(
        &mut self,
        tool: &ToolItem,
        theme: &Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(gate) = tool.gate.clone() else {
            return gpui::Empty.into_any_element();
        };
        let target = approval_target(&tool.call);
        // cwd metadata comes from the chat row; an override (subagent) doc
        // has none and omits the prefix.
        let cwd = if self.doc_override.is_none() {
            self.state
                .read(cx)
                .selected_chat_row()
                .and_then(|chat| chat.cwd.clone())
        } else {
            None
        };
        let meta = approval_meta(cwd.as_deref());
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
                    .when_some(gate.origin, |card, origin| {
                        let chat_id = self.chat_id.clone().unwrap_or_default();
                        let open = super::TranscriptEvent::OpenSubagent {
                            chat_id,
                            doc_id: origin.doc_id.clone(),
                            title: origin.label.clone(),
                            frozen: false,
                        };
                        let keyboard_open = open.clone();
                        card.child(
                            div()
                                .id(format!("approval-source-{}", gate.id))
                                .debug_selector(|| "approval-subagent-source".to_string())
                                .role(gpui::Role::Button)
                                .aria_label("Open subagent")
                                .focusable()
                                .tab_index(0)
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_color(theme.text_muted)
                                .hover(|el| el.bg(theme.ink(0.05)))
                                .focus(|el| el.bg(theme.ink(0.09)))
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(open.clone())))
                                .on_key_down(cx.listener(move |_, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        cx.stop_propagation();
                                        cx.emit(keyboard_open.clone());
                                    }
                                }))
                                .child(SharedString::from(format!(
                                    "Open subagent: {}",
                                    origin.label
                                ))),
                        )
                    })
                    // The gated target leads: a bare mono line — no
                    // framing box; the strip's hairlines are the only chrome.
                    .child(
                        div()
                            .w_full()
                            .font_family(theme.font_mono.clone())
                            .text_size(crate::typography::ui_rems(12.5))
                            .line_height(px(18.0))
                            .text_color(theme.text)
                            .child(SharedString::from(target)),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .line_height(px(15.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(meta)),
                    )
                    // The verdict moved to the composer's approval bar; the
                    // strip marks WHAT is gated and points at it.
                    .child(
                        div()
                            .mt(px(8.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_faint)
                            .child("Waiting for your approval — answer in the composer below"),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::RowKind;
    use super::*;
    use holt_doc::MessageRole;

    fn tool_part(id: &str, gate: Option<ToolGate>) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Exec {
                command: "cargo test".into(),
            },
            is_error: false,
            resolved: false,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            gate,
        }
    }

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

    fn pending_gate(id: &str) -> ToolGate {
        ToolGate {
            origin: None,
            id: id.into(),
            state: ToolGateState::Pending,
        }
    }

    #[test]
    fn pending_gate_scans_from_the_tail() {
        let settled = ToolGate {
            origin: None,
            id: "old".into(),
            state: ToolGateState::Settled {
                verdict: GateVerdict::Allowed,
            },
        };
        let transcript = vec![
            entry("m1", vec![tool_part("p1", Some(pending_gate("g1")))]),
            entry(
                "m2",
                vec![
                    tool_part("p2", Some(settled)),
                    tool_part("p3", Some(pending_gate("g2"))),
                ],
            ),
        ];
        // Latest pending wins; settled gates never report.
        assert_eq!(
            pending_approval_gate(&transcript).map(|g| g.id),
            Some("g2".to_string())
        );
        assert!(pending_approval_gate(&[]).is_none());
        let settled_only = vec![entry("m1", vec![tool_part("p1", None)])];
        assert!(pending_approval_gate(&settled_only).is_none());
    }

    #[test]
    fn targets_render_with_a_shell_prefix_only_for_bash() {
        assert_eq!(
            approval_target(&ToolCall::Exec {
                command: "cargo test".into()
            }),
            "$ cargo test".to_string()
        );
        assert_eq!(
            approval_target(&ToolCall::WriteFile {
                path: "src/main.rs".into(),
                content: None,
            }),
            "src/main.rs".to_string()
        );
        assert_eq!(
            approval_target(&ToolCall::ApplyPatch { path: None }),
            "workspace".to_string()
        );
    }

    #[test]
    fn verdict_chips_cover_every_flavor() {
        let cases: [(GateVerdict, &str, VerdictTint); 9] = [
            (GateVerdict::Allowed, "✓ Approved", VerdictTint::Neutral),
            (
                GateVerdict::AlwaysAllowed,
                "✓ Always allowed",
                VerdictTint::Neutral,
            ),
            (
                GateVerdict::Exempted,
                "⚡ Prefix exempt · auto-passed",
                VerdictTint::Neutral,
            ),
            (
                GateVerdict::ReviewPassed,
                "👁 Auto-review · passed",
                VerdictTint::Neutral,
            ),
            (
                GateVerdict::ReviewRejected { reason: None },
                "👁 Auto-review · rejected",
                VerdictTint::Danger,
            ),
            (
                GateVerdict::ReviewRejected {
                    reason: Some("no tests".into()),
                },
                "👁 Auto-review · rejected · \"no tests\"",
                VerdictTint::Danger,
            ),
            (
                GateVerdict::Denied { note: None },
                "⊘ Denied",
                VerdictTint::Danger,
            ),
            (
                GateVerdict::Denied {
                    note: Some("not now".into()),
                },
                "⊘ Denied · \"not now\"",
                VerdictTint::Danger,
            ),
            (GateVerdict::Aborted, "⊘ Interrupted", VerdictTint::Danger),
        ];
        for (verdict, text, tint) in cases {
            assert_eq!(verdict_chip(&verdict), (text.to_string(), tint));
        }
    }

    /// Fold state around the card: the group above a TAIL pending gate keeps
    /// the live-tail `auto_open` it would have had without the gate — the
    /// turn is paused, not finished, and a running tool's group renders
    /// expanded. A stale pending gate mid-entry flushes the group normally.
    #[test]
    fn tail_pending_gate_keeps_the_group_above_live() {
        use crate::markdown::parser::{BlockTree, parse_full};
        use std::sync::Arc;

        let streaming = |parts: Vec<MessagePart>| SessionMessageEntry {
            status: Some(holt_doc::MessageStatus::Streaming),
            ..entry("m1", parts)
        };
        let mut parse = |_: &str, text: &str| Arc::new(parse_full(text)) as Arc<BlockTree>;

        // Tail gate: [group(p1), approval(p2)] — the group stays auto_open.
        let rows = crate::transcript::rows_for_entry(
            &streaming(vec![
                tool_part("p1", None),
                tool_part("p2", Some(pending_gate("g1"))),
            ]),
            false,
            &mut parse,
        );
        assert_eq!(rows.len(), 2);
        let RowKind::ToolGroup { auto_open, .. } = &rows[0].kind else {
            panic!("expected the group row");
        };
        assert!(
            *auto_open,
            "the group above a tail pending gate stays the live tail"
        );
        assert!(matches!(rows[1].kind, RowKind::Approval { .. }));

        // Mid-entry gate: the flush is ordinary — the group above collapses,
        // the group after the gate owns the tail and opens instead.
        let rows = crate::transcript::rows_for_entry(
            &streaming(vec![
                tool_part("p1", None),
                tool_part("p2", Some(pending_gate("g1"))),
                tool_part("p3", None),
            ]),
            false,
            &mut parse,
        );
        assert_eq!(rows.len(), 3);
        let RowKind::ToolGroup { auto_open, .. } = &rows[0].kind else {
            panic!("expected the group row");
        };
        assert!(!*auto_open, "a mid-entry gate flushes the group normally");
        let RowKind::ToolGroup { auto_open, .. } = &rows[2].kind else {
            panic!("expected the trailing group row");
        };
        assert!(*auto_open, "the group owning the entry tail opens");
    }

    /// Row model: a PENDING gate splices its own Approval row (never a
    /// foldable group); the settle replays the same part into the ordinary
    /// group, where the chip carries the verdict.
    #[test]
    fn pending_gate_splits_its_own_row_then_settles_into_the_group() {
        use crate::markdown::parser::{BlockTree, parse_full};
        use std::sync::Arc;

        fn gated_part(id: &str, state: ToolGateState) -> MessagePart {
            tool_part(
                id,
                Some(ToolGate {
                    origin: None,
                    id: format!("gate-{id}"),
                    state,
                }),
            )
        }

        let mut parse = |_: &str, text: &str| Arc::new(parse_full(text)) as Arc<BlockTree>;
        let pending = entry(
            "m1",
            vec![
                tool_part("p1", None),
                gated_part("p2", ToolGateState::Pending),
                tool_part("p3", None),
            ],
        );
        let rows = crate::transcript::rows_for_entry(&pending, false, &mut parse);
        // The pending gate flushes the group on both sides: [group(p1),
        // approval(p2), group(p3)].
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].kind, RowKind::ToolGroup { .. }));
        let RowKind::Approval { tool } = &rows[1].kind else {
            panic!("expected the approval row");
        };
        assert_eq!(
            tool.gate.as_ref().map(|gate| gate.id.as_str()),
            Some("gate-p2")
        );
        assert_eq!(rows[1].id.as_ref(), "m1#p2");
        assert!(matches!(rows[2].kind, RowKind::ToolGroup { .. }));

        let settled = entry(
            "m1",
            vec![
                tool_part("p1", None),
                gated_part(
                    "p2",
                    ToolGateState::Settled {
                        verdict: GateVerdict::Denied {
                            note: Some("not now".into()),
                        },
                    },
                ),
            ],
        );
        let rows = crate::transcript::rows_for_entry(&settled, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected one group");
        };
        assert!(matches!(
            tools[1].gate.as_ref().map(|gate| &gate.state),
            Some(ToolGateState::Settled { .. })
        ));
    }

    /// Entity + render test: the pending gate lands as an Approval row; the
    /// note editor opens/submits/closes keyed by approval id; the settle
    /// prunes a stale editor; and the whole transcript draws in both states
    /// (strip while pending, verdict chip once settled).
    #[gpui::test]
    fn child_approval_opens_its_source_transcript(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        use std::{cell::RefCell, rc::Rc};
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let (transcript, cx) = cx.add_window_view(|_, cx| Transcript::new(state.clone(), cx));
        let opened = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|_, cx| {
            let opened = opened.clone();
            cx.subscribe(
                &transcript,
                move |_, event: &super::super::TranscriptEvent, _| {
                    opened.borrow_mut().push(event.clone());
                },
            )
        });
        let mut gate = pending_gate("child-gate");
        gate.origin = Some(holt_doc::parts::SubagentOrigin {
            doc_id: "child-doc".into(),
            label: "Inspect assigned files".into(),
        });
        state.update(cx, |state, cx| {
            state
                .transcript
                .push(entry("m1", vec![tool_part("write", Some(gate))]));
            cx.notify();
        });
        transcript.update(cx, |this, cx| {
            this.chat_id = Some("chat-1".into());
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds("approval-subagent-source")
            .expect("visible source button");
        cx.simulate_click(bounds.center(), Default::default());
        assert!(
            matches!(opened.borrow().last(), Some(super::super::TranscriptEvent::OpenSubagent { chat_id, doc_id, frozen: false, .. }) if chat_id == "chat-1" && doc_id == "child-doc")
        );
    }

    /// Entity + render test: the pending gate lands as an Approval row and
    /// the marker strip draws; once settled, the row folds back into the
    /// tool group carrying the verdict chip.
    #[gpui::test]
    fn approval_rows_render_pending_strip_and_settle(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));

        state.update(cx, |s, cx| {
            s.transcript
                .push(entry("m1", vec![tool_part("p1", Some(pending_gate("g1")))]));
            cx.notify();
        });
        transcript.update(cx, |this, _| {
            assert!(
                this.rows
                    .iter()
                    .any(|row| matches!(row.kind, RowKind::Approval { .. }))
            );
        });
        // Draw the pending marker strip (mono lead line, meta, bar hint).
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| transcript.clone().into_any_element(),
        );

        // The settle folds the Approval row back into a tool group.
        state.update(cx, |s, cx| {
            s.transcript[0] = entry(
                "m1",
                vec![tool_part(
                    "p1",
                    Some(ToolGate {
                        origin: None,
                        id: "g1".into(),
                        state: ToolGateState::Settled {
                            verdict: GateVerdict::ReviewPassed,
                        },
                    }),
                )],
            );
            cx.notify();
        });
        transcript.update(cx, |this, _| {
            assert!(
                !this
                    .rows
                    .iter()
                    .any(|row| matches!(row.kind, RowKind::Approval { .. }))
            );
        });
        // Draw the settled verdict chip.
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| transcript.clone().into_any_element(),
        );
    }
}
