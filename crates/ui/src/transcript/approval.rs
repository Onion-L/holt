//! The permission Approval surface (ADR-0014): a pending confirm-changes
//! gate renders as a flat strip in the transcript flow — no card, no nested
//! boxes, just top/bottom hairlines. The gated command/path leads as a bare
//! mono line, the working directory follows as faint metadata, and the four
//! verdict affordances (Allow once / Always allow · this session / Deny /
//! Note…) sit bottom-right. Settled gates render their verdict as a small
//! marker on the ordinary tool chip ([`verdict_chip`]). The strip speaks in
//! the neutral scheme (user call: no accent, no amber) — `danger` only for
//! denials.
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
        // The four verdict affordances, placed at the strip's bottom-right.
        // Small ghost-family buttons (the strip is compact chrome); only
        // Deny carries hue.
        // Allow once — the primary action in the app's subtle-raised idiom
        // (wizard picked-option language: faint ink plate + hairline), not
        // the dialogs' solid plate.
        let allow_once = div()
            .id(format!("approval-once-{id_once}"))
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
                this.resolve_approval(id_once.clone(), ApprovalVerdict::Allow, cx);
            }))
            .child("Allow once");
        // Always allow · this session — a neutral ghost like Note…, but a
        // solid hairline so the four buttons read as one family.
        let always_allow = div()
            .id(format!("approval-always-{id_always}"))
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
                this.resolve_approval(id_always.clone(), ApprovalVerdict::AlwaysAllow, cx);
            }))
            .child("Always allow · this session");
        // Deny — restrained danger (the strip's only hue: the app's error
        // language).
        let deny = div()
            .id(format!("approval-deny-{id_deny}"))
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
            .on_click(cx.listener(move |this, _, _, cx| {
                this.resolve_approval(id_deny.clone(), ApprovalVerdict::Deny { note: None }, cx);
            }))
            .child("Deny");
        // Note… — dashed ghost; opens the note editor.
        let note = div()
            .id(format!("approval-note-{id_note}"))
            .flex_none()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(8.0))
            .border_1()
            .border_dashed()
            .border_color(theme.hairline(0.14))
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|el| el.bg(theme.ink(0.05)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_approval_note(id_note.clone(), window, cx);
            }))
            .child(if note_open { "Hide note" } else { "Note…" });
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
                                .id(format!("approval-source-{}", approval_id))
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
                    // The verdict affordances, bottom-right; the row wraps
                    // under narrow widths.
                    .child(
                        div()
                            .mt(px(8.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .flex_wrap()
                            .gap(px(6.0))
                            .child(allow_once)
                            .child(always_allow)
                            .child(deny)
                            .child(note),
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

    /// The expanding note editor at the strip's foot: the denial reason
    /// goes back to the model as the call's error result. Enter submits
    /// (= Deny with note), Escape cancels the editor WITHOUT reaching the
    /// composer's Esc-interrupt.
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
            .mt(px(10.0))
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
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.danger.opacity(0.3))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger_muted)
                        .cursor_pointer()
                        .hover(|el| el.bg(theme.danger.opacity(0.06)))
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
    /// and the doc's settled gate is what settles the strip). The note
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
        // Draw the pending strip (mono lead line, four affordances).
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
