//! The approval bar (ADR-0014, prototype 4's variant 1): while a
//! confirm-changes gate pends, the composer's pill is replaced by this
//! panel — the gated target as a bare mono lead line, then a flat
//! keyboard-first option list (Allow once / Always allow · this session /
//! Deny) whose trailing row is the free-text denial note. The transcript
//! keeps an in-flow marker strip (`transcript::approval`); the verdict
//! itself rides the shared `ResolveApproval` channel.
//!
//! Keyboard contract: the bar's own focus handle owns the keyboard by
//! default (stamped on open — arrows/Enter/digits never reach the shared
//! input's caret bindings), arrows move the cursor, Enter on an option
//! resolves it, Enter on the note row focuses the input, and Enter there
//! (the input's Submit) denies with the typed note. Escape is NOT handled
//! here — it bubbles to the composer root's Turn interrupt. Pure state
//! (the option model, the cursor) is unit-tested; the gpui glue only
//! feeds it keys and clicks.

use super::Composer;

use gpui::{Context, KeyDownEvent, SharedString, Window, div, prelude::*, px};

use holt_proto::{ApprovalVerdict, ToolCall};

use crate::motion;
use crate::theme::Theme;
use crate::transcript::{
    approval_cwd_line, approval_target, pending_approval_tool, resolve_approval,
};

// ---------------------------------------------------------------------------
// Pure model
// ---------------------------------------------------------------------------

/// One selectable option row: the verdict it resolves with, its label,
/// and whether it speaks in the denial's danger tint (the surface's only
/// hue, per `transcript::approval`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalOption {
    pub label: &'static str,
    pub verdict: ApprovalVerdict,
    pub danger: bool,
}

/// What the bar renders for one pending gate, derived from the gated call.
pub(crate) struct ApprovalPrompt {
    pub title: &'static str,
    pub target: String,
    pub options: Vec<ApprovalOption>,
}

/// The bar's content for one gated call: the kind-specific title, the mono
/// target line, and the three verdict options. The trailing note row is
/// NOT an option — it is the free-text input.
pub(crate) fn approval_prompt(call: &ToolCall) -> ApprovalPrompt {
    let title = match call {
        ToolCall::Exec { .. } => "Run this command?",
        ToolCall::WriteFile { .. } => "Write this file?",
        ToolCall::EditFile { .. } => "Edit this file?",
        ToolCall::ApplyPatch { .. } => "Apply this patch?",
        _ => "Allow this action?",
    };
    ApprovalPrompt {
        title,
        target: approval_target(call),
        options: vec![
            ApprovalOption {
                label: "Allow once",
                verdict: ApprovalVerdict::Allow,
                danger: false,
            },
            ApprovalOption {
                label: "Always allow · this session",
                verdict: ApprovalVerdict::AlwaysAllow,
                danger: false,
            },
            ApprovalOption {
                label: "Deny",
                verdict: ApprovalVerdict::Deny { note: None },
                danger: true,
            },
        ],
    }
}

/// The bar's cursor: rows `0..options.len()` are the verdict options; row
/// `options.len()` is the trailing note input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalBar {
    pub approval_id: String,
    pub selection: usize,
    /// Total cursor rows (options + the note row), stamped at open.
    rows: usize,
}

impl ApprovalBar {
    pub(crate) fn new(approval_id: String, options: usize) -> Self {
        Self {
            approval_id,
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
    /// Confirm the cursor row: an option resolves with its verdict; the
    /// note row hands the shared input focus for the denial note.
    pub(super) fn approval_bar_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.approval_bar.clone() else {
            return;
        };
        if bar.selection == bar.note_row() {
            let handle = self.input.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            return;
        }
        let Some((call, _)) = pending_approval_tool(&self.state.read(cx).transcript) else {
            return;
        };
        let prompt = approval_prompt(&call);
        let Some(option) = prompt.options.get(bar.selection) else {
            return;
        };
        self.resolve_approval_bar(option.verdict.clone(), cx);
    }

    /// Resolve the pending gate and retire the bar. Suppression
    /// (`answered_approvals`) keeps the bar down until the doc frame marks
    /// the gate settled — the wizard's `answered_requests` mirror. The
    /// borrowed input hands back its identity (text and placeholder).
    pub(super) fn resolve_approval_bar(
        &mut self,
        verdict: ApprovalVerdict,
        cx: &mut Context<Self>,
    ) {
        let Some(bar) = self.approval_bar.take() else {
            return;
        };
        self.answered_approvals.insert(bar.approval_id.clone());
        self.input.update(cx, |input, cx| {
            input.set_text("", cx);
            input.set_placeholder("Do anything…", cx);
        });
        resolve_approval(&self.state, bar.approval_id, verdict, cx);
        cx.notify();
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

    /// The panel, rendered in place of the pill while a gate pends (the
    /// wizard's chrome: the same floating pill — `rounded-[26px]` hairline
    /// over a faint wash). `None` when no pending gate needs answering.
    pub(super) fn render_approval_bar(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let bar = self.approval_bar.clone()?;
        let (call, cwd) = {
            let state = self.state.read(cx);
            let (call, _) = pending_approval_tool(&state.transcript)?;
            let cwd = state.selected_chat_row().and_then(|chat| chat.cwd.clone());
            (call, cwd)
        };
        let prompt = approval_prompt(&call);
        let cwd_line = cwd.as_deref().map(approval_cwd_line);
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

        let options =
            prompt.options.iter().enumerate().map(|(ix, option)| {
                let selected = ix == selection;
                let verdict = option.verdict.clone();
                row_frame(selected, format!("approval-bar-option-{ix}"))
                    .id(("approval-bar-option", ix))
                    .on_hover(motion::hover_listener(format!("approval-bar-option-{ix}")))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.resolve_approval_bar(verdict.clone(), cx)
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

        // The trailing row is the free-text denial note: the shared
        // composer input (the wizard's borrowed-input pattern). Enter on
        // the cursor row focuses it; Enter inside denies with the note.
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
                        // The gated target leads: a bare mono line — the
                        // strip's idiom, no framing box.
                        .child(
                            div()
                                .mt(px(8.0))
                                .w_full()
                                .font_family(theme.font_mono.clone())
                                .text_size(crate::typography::ui_rems(12.5))
                                .line_height(px(18.0))
                                .text_color(theme.text)
                                .child(SharedString::from(prompt.target)),
                        )
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
    fn prompt_titles_and_targets_follow_the_call_kind() {
        let exec = approval_prompt(&ToolCall::Exec {
            command: "cargo test".into(),
        });
        assert_eq!(exec.title, "Run this command?");
        assert_eq!(exec.target, "$ cargo test");
        assert_eq!(exec.options.len(), 3);

        let write = approval_prompt(&ToolCall::WriteFile {
            path: "src/main.rs".into(),
            content: None,
        });
        assert_eq!(write.title, "Write this file?");
        assert_eq!(write.target, "src/main.rs");

        let patch = approval_prompt(&ToolCall::ApplyPatch { path: None });
        assert_eq!(patch.title, "Apply this patch?");
        assert_eq!(patch.target, "workspace");
    }

    #[test]
    fn the_option_verdicts_map_to_the_gate_contract() {
        let prompt = approval_prompt(&ToolCall::Exec {
            command: "ls".into(),
        });
        assert_eq!(prompt.options[0].verdict, ApprovalVerdict::Allow);
        assert_eq!(prompt.options[1].verdict, ApprovalVerdict::AlwaysAllow);
        assert_eq!(
            prompt.options[2].verdict,
            ApprovalVerdict::Deny { note: None }
        );
        assert!(prompt.options[2].danger);
        assert!(!prompt.options[0].danger);
    }

    #[test]
    fn the_cursor_clamps_jumps_and_finds_the_note_row() {
        let mut bar = ApprovalBar::new("g1".into(), 3);
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
        use holt_doc::{MessagePart, SessionMessageEntry, ToolGate};
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
            assert_eq!(bar.approval_id, "g1");
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
                this.approval_bar
                    .as_ref()
                    .map(|bar| bar.approval_id.as_str()),
                Some("g2")
            );
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
