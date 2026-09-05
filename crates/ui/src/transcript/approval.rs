//! The permission Approval surface (ADR-0014, prototype 3-A): a pending
//! confirm-changes gate renders as a card in the transcript flow — pulsing
//! header, the gated command/path in a mono block with a danger (bash) or
//! warning (write/edit) left edge, the working directory as metadata, and
//! the four verdict affordances (Allow once / Always allow · this session /
//! Deny / Note…). Settled gates render their verdict as a small marker on
//! the ordinary tool chip ([`verdict_chip`]).
//!
//! Interactive state (the note editor) lives on the `Transcript` entity
//! keyed by approval id — never in `RowKind`, so a row re-splice can't
//! drop a half-written note.

use std::collections::HashSet;

use gpui::{
    AnyElement, Context, Entity, Focusable as _, Hsla, KeyDownEvent, SharedString, Window, div,
    prelude::*, px,
};

use holt_doc::{GateVerdict, MessagePart, SessionMessageEntry, ToolGate, ToolGateState};
use holt_proto::{ApprovalVerdict, ToolCall};
use holt_rpc::methods;

use super::model::{RowKind, ToolItem, skill_file_display};
use super::{ApprovalNote, Transcript, tool_chip_content};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion;
use crate::theme::Theme;

/// The latest still-pending gate in a transcript, if any — the Esc-interrupt
/// hint and the composer's Esc handler key off this.
pub fn pending_approval_gate(transcript: &[SessionMessageEntry]) -> Option<ToolGate> {
    transcript
        .iter()
        .rev()
        .flat_map(|entry| entry.parts.iter().rev())
        .find_map(|part| match part {
            MessagePart::Tool {
                gate: Some(gate), ..
            } if gate.state == ToolGateState::Pending => Some(gate.clone()),
            _ => None,
        })
}

/// The card header's tool noun (prototype 3-A: "Waiting for approval · bash").
pub fn approval_tool_name(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Exec { .. } => "bash",
        ToolCall::WriteFile { .. } => "write",
        ToolCall::EditFile { .. } => "edit",
        _ => tool_chip_content(call).0,
    }
}

/// The gated target rendered in the mono block, plus whether it carries the
/// DANGER left edge (execution risk — bash) vs the warning edge (file
/// writes). Bash commands get the shell's `$ ` prefix.
pub fn approval_target(call: &ToolCall) -> (String, bool) {
    match call {
        ToolCall::Exec { command } => (format!("$ {command}"), true),
        ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => (path.clone(), false),
        ToolCall::ApplyPatch { path } => {
            (path.clone().unwrap_or_else(|| "workspace".into()), false)
        }
        _ => (tool_chip_content(call).1, false),
    }
}

/// Marker color language for a settled verdict: green for passes, amber for
/// the automatic exemption, red for every form of rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictTint {
    Success,
    Warning,
    Danger,
}

/// The settled verdict's compact marker text + tint (prototype 3-A's chip
/// suffixes). A denial/rejection carries the note/reason when there was one
/// — that text is the reason the model received, so the transcript shows it.
pub fn verdict_chip(verdict: &GateVerdict) -> (String, VerdictTint) {
    match verdict {
        GateVerdict::Allowed => ("✓ Approved".to_string(), VerdictTint::Success),
        GateVerdict::AlwaysAllowed => ("✓ Always allowed".to_string(), VerdictTint::Success),
        GateVerdict::Exempted => (
            "⚡ Prefix exempt · auto-passed".to_string(),
            VerdictTint::Warning,
        ),
        GateVerdict::ReviewPassed => ("👁 Auto-review · passed".to_string(), VerdictTint::Success),
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
        VerdictTint::Success => theme.success,
        VerdictTint::Warning => theme.warning,
        VerdictTint::Danger => theme.danger,
    }
}

impl Transcript {
    /// The pending-approval card (prototype 3-A). Styled after the
    /// transcript's error chip: a translucent tinted fill with a soft border
    /// — never a shadow behind translucency.
    pub(super) fn render_approval_card(
        &mut self,
        row_id: &SharedString,
        tool: &ToolItem,
        theme: &Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(gate) = tool.gate.clone() else {
            return gpui::Empty.into_any_element();
        };
        let approval_id = gate.id;
        let tool_name = approval_tool_name(&tool.call);
        let (target, danger_edge) = approval_target(&tool.call);
        let edge = if danger_edge {
            theme.danger
        } else {
            theme.warning
        };
        let pulse =
            motion::pulse_wave(motion::pulse_delta(&motion::HOLT_PULSE, cx.entity_id(), cx));
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
        let meta = match cwd {
            Some(cwd) => format!(
                "cwd: {} · Confirm changes: commands run only after you approve",
                skill_file_display(&cwd)
            ),
            None => "Confirm changes: commands run only after you approve".to_string(),
        };
        let note_input = self
            .approval_notes
            .get(&approval_id)
            .map(|note| note.input.clone());
        let note_open = note_input.is_some();
        let id_once = approval_id.clone();
        let id_always = approval_id.clone();
        let id_deny = approval_id.clone();
        let id_note = approval_id.clone();
        div()
            .py(px(4.0))
            .w_full()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .overflow_hidden()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(theme.warning.opacity(0.35))
                    .bg(theme.warning.opacity(0.05))
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    // Header: pulsing dot + "Waiting for approval · {tool}".
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .size(px(7.0))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(theme.warning)
                                    .opacity(0.25 + 0.75 * pulse),
                            )
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.warning_muted)
                                    .child(SharedString::from(format!(
                                        "Waiting for approval · {tool_name}"
                                    ))),
                            ),
                    )
                    // The gated target: mono block, left edge red for bash
                    // (execution risk), amber for file targets.
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .overflow_hidden()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(edge.opacity(0.3))
                            .bg(theme.ink(0.045))
                            .child(div().w(px(3.0)).flex_none().bg(edge.opacity(0.7)))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .px(px(12.0))
                                    .py(px(10.0))
                                    .font_family(theme.font_mono.clone())
                                    .text_size(crate::typography::ui_rems(12.5))
                                    .line_height(px(18.0))
                                    .text_color(theme.text)
                                    .child(SharedString::from(target)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .line_height(px(15.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(meta)),
                    )
                    // The four verdict affordances (prototype 3-A).
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(8.0))
                            // Allow once — the primary action (house style:
                            // ink fill, on_solid label).
                            .child(
                                div()
                                    .id(format!("approval-once-{id_once}"))
                                    .flex_none()
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(7.0))
                                    .bg(theme.text)
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.on_solid)
                                    .cursor_pointer()
                                    .hover(|el| el.opacity(0.9))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.resolve_approval(
                                            id_once.clone(),
                                            ApprovalVerdict::Allow,
                                            cx,
                                        );
                                    }))
                                    .child("Allow once"),
                            )
                            // Always allow · this session — amber outline (a
                            // grant, not a one-off).
                            .child(
                                div()
                                    .id(format!("approval-always-{id_always}"))
                                    .flex_none()
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(7.0))
                                    .border_1()
                                    .border_color(theme.warning.opacity(0.5))
                                    .text_color(theme.warning)
                                    .cursor_pointer()
                                    .hover(|el| el.bg(theme.warning.opacity(0.08)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.resolve_approval(
                                            id_always.clone(),
                                            ApprovalVerdict::AlwaysAllow,
                                            cx,
                                        );
                                    }))
                                    .child("Always allow · this session"),
                            )
                            // Deny — red outline.
                            .child(
                                div()
                                    .id(format!("approval-deny-{id_deny}"))
                                    .flex_none()
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(7.0))
                                    .border_1()
                                    .border_color(theme.danger.opacity(0.4))
                                    .text_color(theme.danger)
                                    .cursor_pointer()
                                    .hover(|el| el.bg(theme.danger.opacity(0.08)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.resolve_approval(
                                            id_deny.clone(),
                                            ApprovalVerdict::Deny { note: None },
                                            cx,
                                        );
                                    }))
                                    .child("Deny"),
                            )
                            // Note… — dashed ghost; opens the note editor.
                            .child(
                                div()
                                    .id(format!("approval-note-{id_note}"))
                                    .flex_none()
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(7.0))
                                    .border_1()
                                    .border_dashed()
                                    .border_color(theme.hairline(0.2))
                                    .text_color(theme.text_muted)
                                    .cursor_pointer()
                                    .hover(|el| el.bg(theme.ink(0.05)))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.toggle_approval_note(id_note.clone(), window, cx);
                                    }))
                                    .child(if note_open { "Hide note" } else { "Note…" }),
                            ),
                    )
                    .when_some(note_input, |card, input| {
                        card.child(self.render_approval_note(
                            row_id,
                            &approval_id,
                            input,
                            theme,
                            cx,
                        ))
                    }),
            )
            .into_any_element()
    }

    /// The expanding note editor under the card's actions (prototype 3-A):
    /// the denial reason goes back to the model as the call's error result.
    /// Enter submits (= Deny with note), Escape cancels the editor WITHOUT
    /// reaching the composer's Esc-interrupt.
    fn render_approval_note(
        &mut self,
        row_id: &SharedString,
        approval_id: &str,
        input: Entity<ComposerInput>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id_for_key = approval_id.to_string();
        let id_for_submit = approval_id.to_string();
        div()
            .id(SharedString::from(format!("approval-note-{approval_id}")))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.approval_notes.remove(&id_for_key);
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
                            "approval-deny-note-{approval_id}-{row_id}"
                        )))
                        .flex_none()
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(7.0))
                        .border_1()
                        .border_color(theme.danger.opacity(0.4))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .hover(|el| el.bg(theme.danger.opacity(0.08)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.submit_approval_note(&id_for_submit, cx);
                        }))
                        .child("Deny with note"),
                ),
            )
            .into_any_element()
    }

    /// Open (or close) the note editor for one approval. The editor entity
    /// lives on the Transcript keyed by approval id; opening focuses it.
    fn toggle_approval_note(
        &mut self,
        approval_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.approval_notes.remove(&approval_id).is_some() {
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            ComposerInput::new(
                "Why denied — sent back to the model, e.g. use pnpm, not npm",
                cx,
            )
        });
        let id = approval_id.clone();
        let events = cx.subscribe(&input, move |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.submit_approval_note(&id, cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let handle = input.read(cx).focus_handle(cx);
        self.approval_notes.insert(
            approval_id,
            ApprovalNote {
                input,
                _events: events,
            },
        );
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Enter in the note editor = Deny with note: a blank note degrades to a
    /// plain deny.
    fn submit_approval_note(&mut self, approval_id: &str, cx: &mut Context<Self>) {
        let note = self
            .approval_notes
            .get(approval_id)
            .map(|note| note.input.read(cx).text().trim().to_string())
            .filter(|note| !note.is_empty());
        self.resolve_approval(approval_id.to_string(), ApprovalVerdict::Deny { note }, cx);
    }

    /// Send the verdict (fire-and-forget: failures are no-ops engine-side,
    /// and the doc's settled gate is what settles the card). The note
    /// editor, if open, closes with the verdict.
    fn resolve_approval(
        &mut self,
        approval_id: String,
        verdict: ApprovalVerdict,
        cx: &mut Context<Self>,
    ) {
        self.approval_notes.remove(&approval_id);
        cx.notify();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
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

    /// Drop note editors whose approval is no longer pending (verdict landed,
    /// chat switched, transcript switched docs) — called once per render.
    pub(super) fn prune_approval_notes(&mut self) {
        if self.approval_notes.is_empty() {
            return;
        }
        let pending: HashSet<&str> = self
            .rows
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::Approval { tool } => tool.gate.as_ref().map(|gate| gate.id.as_str()),
                _ => None,
            })
            .collect();
        self.approval_notes
            .retain(|id, _| pending.contains(id.as_str()));
    }
}

#[cfg(test)]
mod tests {
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
            id: id.into(),
            state: ToolGateState::Pending,
        }
    }

    #[test]
    fn pending_gate_scans_from_the_tail() {
        let settled = ToolGate {
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
    fn tool_names_match_the_prototype() {
        assert_eq!(
            approval_tool_name(&ToolCall::Exec {
                command: "ls".into()
            }),
            "bash"
        );
        assert_eq!(
            approval_tool_name(&ToolCall::WriteFile {
                path: "a.rs".into(),
                content: None,
            }),
            "write"
        );
        assert_eq!(
            approval_tool_name(&ToolCall::EditFile {
                path: "a.rs".into(),
                old_string: None,
                new_string: None,
            }),
            "edit"
        );
        assert_eq!(
            approval_tool_name(&ToolCall::ReadFile {
                path: "a.rs".into()
            }),
            tool_chip_content(&ToolCall::ReadFile {
                path: "a.rs".into()
            })
            .0
        );
    }

    #[test]
    fn targets_carry_the_danger_edge_only_for_bash() {
        assert_eq!(
            approval_target(&ToolCall::Exec {
                command: "cargo test".into()
            }),
            ("$ cargo test".to_string(), true)
        );
        assert_eq!(
            approval_target(&ToolCall::WriteFile {
                path: "src/main.rs".into(),
                content: None,
            }),
            ("src/main.rs".to_string(), false)
        );
        assert_eq!(
            approval_target(&ToolCall::ApplyPatch { path: None }),
            ("workspace".to_string(), false)
        );
    }

    #[test]
    fn verdict_chips_cover_every_flavor() {
        let cases: [(GateVerdict, &str, VerdictTint); 9] = [
            (GateVerdict::Allowed, "✓ Approved", VerdictTint::Success),
            (
                GateVerdict::AlwaysAllowed,
                "✓ Always allowed",
                VerdictTint::Success,
            ),
            (
                GateVerdict::Exempted,
                "⚡ Prefix exempt · auto-passed",
                VerdictTint::Warning,
            ),
            (
                GateVerdict::ReviewPassed,
                "👁 Auto-review · passed",
                VerdictTint::Success,
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
    /// (card while pending, verdict chip once settled).
    #[gpui::test]
    fn approval_rows_note_editor_and_render(cx: &mut gpui::TestAppContext) {
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
            assert!(this.approval_notes.is_empty());
        });
        // Draw the pending card (pulse, mono block, four affordances).
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| transcript.clone().into_any_element(),
        );

        // Note… opens the editor; toggling closes it.
        cx.update(|window, cx| {
            transcript.update(cx, |this, cx| {
                this.toggle_approval_note("g1".into(), window, cx);
            });
        });
        transcript.update(cx, |this, _| {
            assert!(this.approval_notes.contains_key("g1"))
        });
        cx.update(|window, cx| {
            transcript.update(cx, |this, cx| {
                this.toggle_approval_note("g1".into(), window, cx);
            });
        });
        transcript.update(cx, |this, _| assert!(this.approval_notes.is_empty()));

        // Enter on a blank note = plain deny; the editor closes with the
        // verdict (the RPC is a no-op without an engine — state-only here).
        cx.update(|window, cx| {
            transcript.update(cx, |this, cx| {
                this.toggle_approval_note("g1".into(), window, cx);
            });
        });
        transcript.update(cx, |this, cx| {
            this.submit_approval_note("g1", cx);
            assert!(this.approval_notes.is_empty());
        });

        // A stale editor is pruned once its approval settles: reopen, settle
        // the gate, and the Approval row folds back into a tool group.
        cx.update(|window, cx| {
            transcript.update(cx, |this, cx| {
                this.toggle_approval_note("g1".into(), window, cx);
            });
        });
        state.update(cx, |s, cx| {
            s.transcript[0] = entry(
                "m1",
                vec![tool_part(
                    "p1",
                    Some(ToolGate {
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
            this.prune_approval_notes();
            assert!(this.approval_notes.is_empty());
        });
        // Draw the settled verdict chip.
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| transcript.clone().into_any_element(),
        );
    }
}
