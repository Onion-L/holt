//! The approval bar: one composer-takeover panel for every user verdict.
//! While something pends, the composer's pill is replaced by a flat
//! keyboard-first option list whose trailing row is a free-text input.
//! Two producers feed it (ADR-0014 / ADR-0025, prototype 4's variant 1):
//!
//! - **Gate** — a confirm-changes gate: the gated target as a bare mono
//!   lead line, then Allow once / Always allow · this session / Deny, the
//!   note row carrying the denial note. Escape interrupts the Turn (it
//!   bubbles to the composer root's handler).
//! - **Plan** — a submitted plan awaiting its verdict: Approve / Reject /
//!   Stay in planning, the note row carrying the rejection feedback. No
//!   target line (the plan document is the transcript card above); Escape
//!   is inert (no Turn is blocked on a plan).
//!
//! The transcript builds no interactive counterpart for either (user
//! call: a duplicated strip reads as noise); verdicts ride the shared
//! `ResolveApproval` / `ResolvePlanApproval` channels.
//!
//! Keyboard contract: the bar's own focus handle owns the keyboard by
//! default (stamped on open — arrows/Enter/digits never reach the shared
//! input's caret bindings), arrows move the cursor, Enter on an option
//! resolves it, Enter on the note row focuses the input, and Enter there
//! (the input's Submit) sends the note. Pure state (the prompt models,
//! the cursor) is unit-tested; the gpui glue only feeds it keys and
//! clicks.

use super::Composer;

use gpui::{App, Context, KeyDownEvent, SharedString, Window, div, prelude::*, px};

use holt_doc::SessionMessageEntry;
use holt_proto::{ApprovalVerdict, ToolCall};

use crate::motion;
use crate::theme::Theme;
use crate::transcript::{
    approval_cwd_line, approval_target, pending_approval_tool, pending_plan_approval,
    resolve_approval, resolve_plan_approval,
};

// ---------------------------------------------------------------------------
// Pure model
// ---------------------------------------------------------------------------

/// Which approval subsystem the open bar answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarKind {
    /// ADR-0014 confirm-changes gate.
    Gate,
    /// ADR-0025 plan approval.
    Plan,
}

/// One option row's resolve payload: a gate verdict, or the plan
/// verdict word (`"approve" | "reject" | "remain"` — rejection feedback
/// arrives separately through the note row).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BarVerdict {
    Gate(ApprovalVerdict),
    Plan(&'static str),
}

/// One selectable option row: its label, its resolve payload, and whether
/// it speaks in the rejection's danger tint (the surface's only hue).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalOption {
    pub label: &'static str,
    pub verdict: BarVerdict,
    pub danger: bool,
}

/// What the bar renders for one pending approval: the title, an optional
/// mono target line (the plan's document is the transcript card, so it
/// has none), the option rows, and the note row's placeholder.
pub(crate) struct ApprovalPrompt {
    pub kind: BarKind,
    pub title: &'static str,
    pub target: Option<String>,
    pub options: Vec<ApprovalOption>,
    pub note_placeholder: &'static str,
}

/// The gate's prompt (ADR-0014): the kind-specific title, the mono target
/// line, and the three verdict options. The trailing note row is NOT an
/// option — it is the free-text input.
pub(crate) fn gate_prompt(call: &ToolCall) -> ApprovalPrompt {
    let title = match call {
        ToolCall::Exec { .. } => "Run this command?",
        ToolCall::WriteFile { .. } => "Write this file?",
        ToolCall::EditFile { .. } => "Edit this file?",
        ToolCall::ApplyPatch { .. } => "Apply this patch?",
        _ => "Allow this action?",
    };
    ApprovalPrompt {
        kind: BarKind::Gate,
        title,
        target: Some(approval_target(call)),
        options: vec![
            ApprovalOption {
                label: "Allow once",
                verdict: BarVerdict::Gate(ApprovalVerdict::Allow),
                danger: false,
            },
            ApprovalOption {
                label: "Always allow · this session",
                verdict: BarVerdict::Gate(ApprovalVerdict::AlwaysAllow),
                danger: false,
            },
            ApprovalOption {
                label: "Deny",
                verdict: BarVerdict::Gate(ApprovalVerdict::Deny { note: None }),
                danger: true,
            },
        ],
        note_placeholder: "Deny with a note…",
    }
}

/// The plan's prompt (ADR-0025): no target line (the submitted plan is
/// the transcript card above) — title, three verdict options, and the
/// rejection-feedback note row.
pub(crate) fn plan_prompt() -> ApprovalPrompt {
    ApprovalPrompt {
        kind: BarKind::Plan,
        title: "Approve this plan?",
        target: None,
        options: vec![
            ApprovalOption {
                label: "Approve",
                verdict: BarVerdict::Plan("approve"),
                danger: false,
            },
            ApprovalOption {
                label: "Reject",
                verdict: BarVerdict::Plan("reject"),
                danger: true,
            },
            ApprovalOption {
                label: "Stay in planning",
                verdict: BarVerdict::Plan("remain"),
                danger: false,
            },
        ],
        note_placeholder: "Reject with feedback…",
    }
}

/// The latest pending approval of either kind, gate first. The two are
/// mutually exclusive in practice (Plan Mode mounts read-only tools, so
/// no mutating call gates while planning) — the order only breaks ties.
pub(crate) enum PendingApproval {
    Gate { call: ToolCall, id: String },
    Plan(String),
}

impl PendingApproval {
    pub(crate) fn from_transcript(transcript: &[SessionMessageEntry]) -> Option<Self> {
        pending_approval_tool(transcript)
            .map(|(call, gate)| PendingApproval::Gate { call, id: gate.id })
            .or_else(|| pending_plan_approval(transcript).map(PendingApproval::Plan))
    }

    pub(crate) fn id(&self) -> &str {
        match self {
            PendingApproval::Gate { id, .. } => id,
            PendingApproval::Plan(key) => key,
        }
    }

    pub(crate) fn prompt(&self) -> ApprovalPrompt {
        match self {
            PendingApproval::Gate { call, .. } => gate_prompt(call),
            PendingApproval::Plan(_) => plan_prompt(),
        }
    }
}

/// The bar's cursor: rows `0..options.len()` are the verdict options; row
/// `options.len()` is the trailing note input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalBar {
    pub id: String,
    pub kind: BarKind,
    pub selection: usize,
    /// Total cursor rows (options + the note row), stamped at open.
    rows: usize,
}

impl ApprovalBar {
    pub(crate) fn new(id: String, kind: BarKind, options: usize) -> Self {
        Self {
            id,
            kind,
            selection: 0,
            rows: options + 1,
        }
    }

    /// The note row's cursor position.
    pub(crate) fn note_row(&self) -> usize {
        self.rows - 1
    }

    pub(crate) fn move_by(&mut self, delta: isize) {
        let next = self.selection as isize + delta;
        self.selection = next.clamp(0, self.rows as isize - 1) as usize;
    }

    /// A bare digit jumps 1..=rows; out of range is ignored.
    pub(crate) fn press_number(&mut self, number: usize) -> bool {
        if number == 0 || number > self.rows {
            return false;
        }
        self.selection = number - 1;
        true
    }
}

// ---------------------------------------------------------------------------
// Composer glue
// ---------------------------------------------------------------------------

impl Composer {
    /// The content model for the open bar, rebuilt from its producer: the
    /// gate's prompt derives from the gated call, the plan's is static.
    fn bar_prompt(&self, cx: &App) -> Option<ApprovalPrompt> {
        let bar = self.approval_bar.as_ref()?;
        match bar.kind {
            BarKind::Gate => {
                let (call, _) = pending_approval_tool(&self.state.read(cx).transcript)?;
                Some(gate_prompt(&call))
            }
            BarKind::Plan => Some(plan_prompt()),
        }
    }

    /// Confirm the cursor row: an option resolves with its verdict; the
    /// note row hands the shared input focus for the note.
    pub(super) fn approval_bar_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.approval_bar.clone() else {
            return;
        };
        if bar.selection == bar.note_row() {
            let handle = self.input.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            return;
        }
        let Some(prompt) = self.bar_prompt(cx) else {
            return;
        };
        let Some(option) = prompt.options.get(bar.selection) else {
            return;
        };
        self.resolve_approval_bar(option.verdict.clone(), None, cx);
    }

    /// Resolve the pending approval and retire the bar. Suppression
    /// (`answered_approvals`) keeps the bar down until the doc frame marks
    /// it settled — the wizard's `answered_requests` mirror. The borrowed
    /// input hands back its identity (text and placeholder).
    pub(super) fn resolve_approval_bar(
        &mut self,
        verdict: BarVerdict,
        plan_feedback: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(bar) = self.approval_bar.take() else {
            return;
        };
        self.answered_approvals.insert(bar.id.clone());
        self.input.update(cx, |input, cx| {
            input.set_text("", cx);
            input.set_placeholder("Do anything…", cx);
        });
        match verdict {
            BarVerdict::Gate(verdict) => resolve_approval(&self.state, bar.id, verdict, cx),
            BarVerdict::Plan(word) => resolve_plan_approval(&self.state, word, plan_feedback, cx),
        }
        cx.notify();
    }

    /// Enter in the bar's note row sends the note with the kind's
    /// negative verdict: the gate's denial (blank = plain deny), the
    /// plan's rejection feedback (blank = plain reject).
    pub(super) fn resolve_bar_note(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.approval_bar.clone() else {
            return;
        };
        let note = self.input.read(cx).text().trim().to_string();
        let note = (!note.is_empty()).then_some(note);
        match bar.kind {
            BarKind::Gate => self.resolve_approval_bar(
                BarVerdict::Gate(ApprovalVerdict::Deny { note }),
                None,
                cx,
            ),
            BarKind::Plan => self.resolve_approval_bar(BarVerdict::Plan("reject"), note, cx),
        }
    }

    /// Keys on the bar's root. The bar's own focus handle owns the
    /// keyboard by default (stamped on open), so arrows/Enter/digits land
    /// here instead of in the shared input's caret bindings. A focused
    /// note input owns its keys instead: arrows are caret movement, Enter
    /// is the input's Submit (the note), digits are text. Escape is
    /// unhandled on purpose — see the module docs.
    pub(super) fn on_approval_bar_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.input.read(cx).focus_handle.is_focused(window) {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "up" || key == "down" {
            if let Some(bar) = self.approval_bar.as_mut() {
                bar.move_by(if key == "up" { -1 } else { 1 });
            }
            cx.stop_propagation();
            cx.notify();
        } else if let Ok(digit) = key.parse::<usize>()
            && (1..=9).contains(&digit)
            && !event.keystroke.modifiers.modified()
        {
            let Some(bar) = self.approval_bar.as_mut() else {
                return;
            };
            if bar.press_number(digit) {
                cx.stop_propagation();
                self.approval_bar_confirm(window, cx);
            }
        } else if key == "enter" {
            self.approval_bar_confirm(window, cx);
            cx.stop_propagation();
        }
    }

    /// The panel, rendered in place of the pill while an approval pends
    /// (the wizard's chrome: the same floating pill — `rounded-[26px]`
    /// hairline over a faint wash). `None` when nothing needs answering.
    pub(super) fn render_approval_bar(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let bar = self.approval_bar.clone()?;
        let prompt = self.bar_prompt(cx)?;
        let cwd_line = match prompt.kind {
            BarKind::Gate => self
                .state
                .read(cx)
                .selected_chat_row()
                .and_then(|chat| chat.cwd.clone())
                .map(|cwd| approval_cwd_line(&cwd)),
            BarKind::Plan => None,
        };
        let selection = bar.selection.min(bar.note_row());
        let note_row = bar.note_row();
        let input_focused = self.input.read(cx).focus_handle.is_focused(window);
        let note_selected = selection == note_row && !input_focused;

        let number_chip = |ix: usize, selected: bool| {
            div()
                .flex_none()
                .size(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .bg(if selected {
                    crate::theme::ink(0.16)
                } else {
                    crate::theme::ink(0.05)
                })
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(if selected {
                    theme.text
                } else {
                    theme.text_muted.opacity(0.6)
                })
                .child(SharedString::from(format!("{}", ix + 1)))
        };

        let row_frame = |selected: bool, key: String| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .px(px(12.0))
                .py(px(7.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(if selected {
                    crate::theme::ink(0.16)
                } else {
                    gpui::transparent_black()
                })
                .bg(if selected {
                    crate::theme::ink(0.09)
                } else {
                    motion::hover_blend(&key, gpui::transparent_black(), crate::theme::ink(0.06))
                })
        };

        let options = prompt.options.iter().enumerate().map(|(ix, option)| {
            let selected = ix == selection;
            let verdict = option.verdict.clone();
            row_frame(selected, format!("approval-bar-option-{ix}"))
                .id(("approval-bar-option", ix))
                .on_hover(motion::hover_listener(format!("approval-bar-option-{ix}")))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.resolve_approval_bar(verdict.clone(), None, cx)
                }))
                .child(number_chip(ix, selected))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(crate::typography::ui_rems(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if option.danger {
                            theme.danger_muted
                        } else if selected {
                            theme.text
                        } else {
                            theme.text.opacity(0.9)
                        })
                        .child(option.label),
                )
        });

        // The trailing row is the free-text note: the shared composer
        // input (the wizard's borrowed-input pattern). Enter on the
        // cursor row focuses it; Enter inside sends the note.
        let note = row_frame(note_selected, "approval-bar-note".to_string())
            .id("approval-bar-note")
            .on_hover(motion::hover_listener("approval-bar-note"))
            .cursor_text()
            .on_click(cx.listener(|this, _, window, cx| {
                let handle = this.input.read(cx).focus_handle.clone();
                window.focus(&handle, cx);
            }))
            .child(number_chip(note_row, note_selected))
            .child(div().flex_1().min_w_0().child(self.input.clone()));

        Some(
            div()
                .id("approval-bar")
                .track_focus(&self.approval_bar_focus)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    this.on_approval_bar_key(event, window, cx)
                }))
                .rounded(px(26.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.input_glass_bg())
                .when(!theme.is_frost(), |el| el.shadow_lg())
                .flex()
                .flex_col()
                .child(
                    div()
                        .px(px(16.0))
                        .pt(px(16.0))
                        .pb(px(12.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(15.0))
                                .line_height(px(20.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(prompt.title),
                        )
                        // The target leads when the producer has one: a
                        // bare mono line — the strip's idiom, no framing
                        // box. The plan's document is the transcript card.
                        .when_some(prompt.target.clone(), |el, target| {
                            el.child(
                                div()
                                    .mt(px(8.0))
                                    .w_full()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(crate::typography::ui_rems(12.5))
                                    .line_height(px(18.0))
                                    .text_color(theme.text)
                                    .child(SharedString::from(target)),
                            )
                        })
                        .when_some(cwd_line, |el, line| {
                            el.child(
                                div()
                                    .mt(px(4.0))
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .line_height(px(15.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(line)),
                            )
                        })
                        .child(
                            div()
                                .mt(px(12.0))
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .children(options)
                                .child(note),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_prompt_titles_and_targets_follow_the_call_kind() {
        let exec = gate_prompt(&ToolCall::Exec {
            command: "cargo test".into(),
        });
        assert_eq!(exec.kind, BarKind::Gate);
        assert_eq!(exec.title, "Run this command?");
        assert_eq!(exec.target.as_deref(), Some("$ cargo test"));
        assert_eq!(exec.options.len(), 3);
        assert_eq!(exec.note_placeholder, "Deny with a note…");

        let write = gate_prompt(&ToolCall::WriteFile {
            path: "src/main.rs".into(),
            content: None,
        });
        assert_eq!(write.title, "Write this file?");
        assert_eq!(write.target.as_deref(), Some("src/main.rs"));

        let patch = gate_prompt(&ToolCall::ApplyPatch { path: None });
        assert_eq!(patch.title, "Apply this patch?");
        assert_eq!(patch.target.as_deref(), Some("workspace"));
    }

    #[test]
    fn the_gate_option_verdicts_map_to_the_gate_contract() {
        let prompt = gate_prompt(&ToolCall::Exec {
            command: "ls".into(),
        });
        assert_eq!(
            prompt.options[0].verdict,
            BarVerdict::Gate(ApprovalVerdict::Allow)
        );
        assert_eq!(
            prompt.options[1].verdict,
            BarVerdict::Gate(ApprovalVerdict::AlwaysAllow)
        );
        assert_eq!(
            prompt.options[2].verdict,
            BarVerdict::Gate(ApprovalVerdict::Deny { note: None })
        );
        assert!(prompt.options[2].danger);
        assert!(!prompt.options[0].danger);
    }

    #[test]
    fn the_plan_prompt_maps_to_the_plan_contract() {
        let prompt = plan_prompt();
        assert_eq!(prompt.kind, BarKind::Plan);
        assert_eq!(prompt.title, "Approve this plan?");
        // No target line: the plan document is the transcript card.
        assert_eq!(prompt.target, None);
        assert_eq!(prompt.note_placeholder, "Reject with feedback…");
        let verdicts: Vec<BarVerdict> = prompt
            .options
            .iter()
            .map(|option| option.verdict.clone())
            .collect();
        assert_eq!(
            verdicts,
            vec![
                BarVerdict::Plan("approve"),
                BarVerdict::Plan("reject"),
                BarVerdict::Plan("remain"),
            ]
        );
        assert!(prompt.options[1].danger, "reject is the danger option");
        assert!(!prompt.options[0].danger && !prompt.options[2].danger);
    }

    #[test]
    fn the_cursor_clamps_jumps_and_finds_the_note_row() {
        let mut bar = ApprovalBar::new("g1".into(), BarKind::Gate, 3);
        assert_eq!(bar.note_row(), 3);
        assert_eq!(bar.selection, 0);
        bar.move_by(-1);
        assert_eq!(bar.selection, 0, "clamped at the top");
        bar.move_by(10);
        assert_eq!(bar.selection, 3, "clamped at the note row");
        assert!(bar.press_number(2));
        assert_eq!(bar.selection, 1);
        assert!(bar.press_number(4));
        assert_eq!(bar.selection, 3);
        assert!(!bar.press_number(5), "out of range ignored");
        assert!(!bar.press_number(0));
        assert_eq!(bar.selection, 3);
    }

    fn gated(state: holt_doc::ToolGateState) -> holt_doc::SessionMessageEntry {
        use holt_doc::{MessagePart, ToolGate};
        SessionMessageEntry {
            id: "m1".into(),
            role: holt_doc::MessageRole::Assistant,
            parts: vec![MessagePart::Tool {
                id: "p1".into(),
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
                gate: Some(ToolGate {
                    origin: None,
                    id: "g1".into(),
                    state,
                }),
            }],
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn plan_pending(state: holt_doc::PlanApprovalState) -> holt_doc::SessionMessageEntry {
        use holt_doc::MessagePart;
        SessionMessageEntry {
            id: "s1".into(),
            role: holt_doc::MessageRole::System,
            parts: vec![MessagePart::PlanApproval {
                id: "p1".into(),
                content: "# The plan".into(),
                state,
            }],
            created_at: 1,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn the_pending_scan_prefers_the_gate_then_the_plan() {
        use holt_doc::{GateVerdict, PlanApprovalVerdict, ToolGateState};
        // Gate and plan both pending: the gate wins the tie.
        let both = vec![
            plan_pending(holt_doc::PlanApprovalState::Pending),
            gated(ToolGateState::Pending),
        ];
        let Some(PendingApproval::Gate { id, .. }) = PendingApproval::from_transcript(&both) else {
            panic!("expected the gate to win")
        };
        assert_eq!(id, "g1");
        // Plan only.
        let plan_only = vec![plan_pending(holt_doc::PlanApprovalState::Pending)];
        let Some(PendingApproval::Plan(key)) = PendingApproval::from_transcript(&plan_only) else {
            panic!("expected the plan")
        };
        assert_eq!(key, "s1#p1");
        // Nothing pending.
        let settled = vec![
            plan_pending(holt_doc::PlanApprovalState::Settled {
                verdict: PlanApprovalVerdict::Approved,
            }),
            gated(ToolGateState::Settled {
                verdict: GateVerdict::Allowed,
            }),
        ];
        assert!(PendingApproval::from_transcript(&settled).is_none());
    }

    #[gpui::test]
    fn the_bar_opens_on_a_pending_gate_and_retires_with_the_verdict(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        use holt_doc::{MessagePart, ToolGateState};
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));

        // A pending gate opens the bar keyed to its approval id, with the
        // keyboard landing on the bar's own focus handle (not the input).
        state.update(cx, |s, cx| {
            s.transcript.push(gated(ToolGateState::Pending));
            cx.notify();
        });
        composer.update(cx, |this, _| {
            let bar = this.approval_bar.as_ref().expect("the bar opened");
            assert_eq!(bar.id, "g1");
            assert_eq!(bar.kind, BarKind::Gate);
            assert_eq!(bar.selection, 0);
            assert!(this.approval_bar_focus_pending);
        });
        // Draw the bar (mono target, options, note row) without panicking.
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| composer.clone().into_any_element(),
        );
        composer.update(cx, |this, _| {
            assert!(
                !this.approval_bar_focus_pending,
                "the first frame consumed the focus stamp"
            );
        });

        // Enter in the (empty) note row denies plainly; the bar retires and
        // the gate stays suppressed until the doc settles it.
        composer.update(cx, |this, cx| {
            this.on_submit(cx);
            assert!(this.approval_bar.is_none());
            assert!(this.answered_approvals.contains("g1"));
            assert!(this.input.read(cx).is_empty());
        });
        composer.update(cx, |this, _| {
            assert!(
                this.approval_bar.is_none(),
                "an answered gate stays suppressed until the doc settles"
            );
        });

        // The settle re-arms the surface for the NEXT gate.
        state.update(cx, |s, cx| {
            s.transcript[0] = gated(ToolGateState::Settled {
                verdict: holt_doc::GateVerdict::Allowed,
            });
            cx.notify();
        });
        composer.update(cx, |this, _| {
            assert!(!this.answered_approvals.contains("g1"));
        });
        state.update(cx, |s, cx| {
            let mut next = gated(ToolGateState::Pending);
            let MessagePart::Tool { gate, .. } = &mut next.parts[0] else {
                panic!()
            };
            gate.as_mut().unwrap().id = "g2".into();
            s.transcript.push(next);
            cx.notify();
        });
        composer.update(cx, |this, _| {
            assert_eq!(
                this.approval_bar.as_ref().map(|bar| bar.id.as_str()),
                Some("g2")
            );
        });
    }

    /// The plan producer: a pending plan approval opens the bar in Plan
    /// kind (no target line in the prompt); the note row's Enter rejects,
    /// and the bar stays suppressed until the card settles.
    #[gpui::test]
    fn the_bar_opens_on_a_pending_plan_and_rejects_from_the_note(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));

        state.update(cx, |s, cx| {
            s.transcript
                .push(plan_pending(holt_doc::PlanApprovalState::Pending));
            cx.notify();
        });
        composer.update(cx, |this, _| {
            let bar = this.approval_bar.as_ref().expect("the bar opened");
            assert_eq!(bar.id, "s1#p1");
            assert_eq!(bar.kind, BarKind::Plan);
        });
        // Draw the plan bar (title + options + note row, no target line).
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| composer.clone().into_any_element(),
        );

        composer.update(cx, |this, cx| {
            this.on_submit(cx);
            assert!(this.approval_bar.is_none());
            assert!(this.answered_approvals.contains("s1#p1"));
        });
        // The settle re-arms the surface.
        state.update(cx, |s, cx| {
            s.transcript[0] = plan_pending(holt_doc::PlanApprovalState::Settled {
                verdict: holt_doc::PlanApprovalVerdict::Remained,
            });
            cx.notify();
        });
        composer.update(cx, |this, _| {
            assert!(!this.answered_approvals.contains("s1#p1"));
        });
    }

    /// Real keystrokes through a window: the bar's focus handle owns the
    /// arrows — gpui names them "up"/"down" (an "arrow*" name never fires,
    /// which is what the first cut got wrong) — and Enter resolves the
    /// cursor option.
    #[gpui::test]
    fn arrows_move_the_cursor_and_enter_resolves(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        use holt_doc::ToolGateState;
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let (composer, cx) = cx.add_window_view(|_window, cx| Composer::new(state.clone(), cx));

        state.update(cx, |s, cx| {
            s.transcript.push(gated(ToolGateState::Pending));
            cx.notify();
        });
        cx.run_until_parked();
        composer.update(cx, |this, _| assert!(this.approval_bar.is_some()));

        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        composer.update(cx, |this, _| {
            assert_eq!(this.approval_bar.as_ref().unwrap().selection, 1)
        });
        cx.simulate_keystrokes("down up");
        cx.run_until_parked();
        composer.update(cx, |this, _| {
            assert_eq!(this.approval_bar.as_ref().unwrap().selection, 1)
        });

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        composer.update(cx, |this, _| {
            assert!(this.approval_bar.is_none());
            assert!(this.answered_approvals.contains("g1"));
        });
    }
}
