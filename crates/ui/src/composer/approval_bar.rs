//! The approval bar: one composer-takeover panel for every user verdict.
//! While something pends, the composer's pill is replaced by a flat
//! keyboard-first option list whose trailing row is a free-text input.
//! Two producers feed it (ADR-0014 / ADR-0025, prototype 4's variant 1):
//!
//! - **Gate** — a confirm-changes gate: the gated target as a bare mono
//!   lead line, then Allow once / Always allow · this session / Deny, the
//!   note row carrying the denial note. Escape interrupts the Turn (it
//!   bubbles to the composer root's handler).
//! - **Plan** — a submitted plan awaiting its verdict: Approve (exits
//!   Plan Mode; the engine enqueues an approval follow-up prompt that
//!   starts the implementation Turn), the note row carrying the revision
//!   feedback (its Enter sends the `reject` verdict with the note, which
//!   keeps planning). No target line (the plan document is the transcript
//!   card above); Escape is inert (no Turn is blocked on a plan).
//! - **Question** — the agent's ask_user card (ADR-0040): the question
//!   as the title, its options as keyboard-first rows (a multi-question
//!   card pages with left/right and a 1/N pager), the note row as the
//!   free-text answer. Escape dismisses the card unanswered — nothing is
//!   blocked, so the bar closes and the card stamps superseded.
//!
//! The transcript builds no interactive counterpart for any of them (user
//! call: a duplicated strip reads as noise); verdicts ride the shared
//! `ResolveApproval` / `ResolvePlanApproval` / `SettleQuestion` /
//! `DismissQuestion` channels.
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
    /// The agent's `ask_user` question (ADR-0040): the options are the
    /// card's enumerated answers, the note row is the free-text answer.
    Question,
}

/// One option row's resolve payload: a gate verdict, the plan
/// verdict word (`"approve" | "reject" | "remain"` — rejection feedback
/// arrives separately through the note row), or the question card's
/// answer text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BarVerdict {
    Gate(ApprovalVerdict),
    Plan(&'static str),
    Question(String),
}

/// One selectable option row: its label, its resolve payload, and whether
/// it speaks in the rejection's danger tint (the surface's only hue).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalOption {
    pub label: SharedString,
    pub verdict: BarVerdict,
    pub danger: bool,
}

/// What the bar renders for one pending approval: the title, an optional
/// mono target line (the plan's document is the transcript card, so it
/// has none), the option rows, and the note row's placeholder.
pub(crate) struct ApprovalPrompt {
    pub kind: BarKind,
    pub title: String,
    pub target: Option<String>,
    pub options: Vec<ApprovalOption>,
    /// The note row's placeholder.
    pub note_placeholder: &'static str,
    /// The question card's pager: the 1-based page and the question count.
    /// `None` on the single-surface kinds.
    pub pager: Option<(usize, usize)>,
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
        title: title.to_string(),
        target: Some(approval_target(call)),
        options: vec![
            ApprovalOption {
                label: "Allow once".into(),
                verdict: BarVerdict::Gate(ApprovalVerdict::Allow),
                danger: false,
            },
            ApprovalOption {
                label: "Always allow · this session".into(),
                verdict: BarVerdict::Gate(ApprovalVerdict::AlwaysAllow),
                danger: false,
            },
            ApprovalOption {
                label: "Deny".into(),
                verdict: BarVerdict::Gate(ApprovalVerdict::Deny { note: None }),
                danger: true,
            },
        ],
        note_placeholder: "Deny with a note…",
        pager: None,
    }
}

/// The plan's prompt (ADR-0025): no target line (the submitted plan is
/// the transcript card above) — title, the Approve option, and the note
/// row carrying the revision feedback.
pub(crate) fn plan_prompt() -> ApprovalPrompt {
    ApprovalPrompt {
        kind: BarKind::Plan,
        title: "Approve this plan?".to_string(),
        target: None,
        options: vec![ApprovalOption {
            label: "Approve".into(),
            verdict: BarVerdict::Plan("approve"),
            danger: false,
        }],
        note_placeholder: "Enter feedback…",
        pager: None,
    }
}

/// The question's prompt for one page (ADR-0040): that question is the
/// title, its enumerated options are the rows, and the note row is the
/// free-text answer. Multi-question cards carry a pager.
pub(crate) fn question_prompt(questions: &[holt_doc::CardQuestion], page: usize) -> ApprovalPrompt {
    let page = page.min(questions.len().saturating_sub(1));
    let current = &questions[page];
    ApprovalPrompt {
        kind: BarKind::Question,
        title: current.question.clone(),
        target: None,
        options: current
            .options
            .iter()
            .cloned()
            .map(|option| ApprovalOption {
                label: option.clone().into(),
                verdict: BarVerdict::Question(option),
                danger: false,
            })
            .collect(),
        note_placeholder: "Answer in words…",
        pager: (questions.len() > 1).then_some((page + 1, questions.len())),
    }
}

/// The latest pending approval of either kind, gate first. The two are
/// mutually exclusive in practice (Plan Mode mounts read-only tools, so
/// no mutating call gates while planning) — the order only breaks ties.
pub(crate) enum PendingApproval {
    Gate {
        call: ToolCall,
        id: String,
        /// The pending gate's note — a forced approval's stored-proposal
        /// summary (ADR-0029); `None` on ordinary approvals.
        note: Option<String>,
    },
    Plan(String),
    /// A pending question card (ADR-0040): the card id answers the RPC,
    /// the questions build the bar one page per question.
    Question {
        card_id: String,
        questions: Vec<holt_doc::CardQuestion>,
    },
}

/// The model-setup apply tool (ADR-0029): the one gated call that is not
/// the write/edit/bash trio.
fn is_model_apply(call: &ToolCall) -> bool {
    matches!(call, ToolCall::Unknown { name, .. } if name == "model_apply")
}

impl PendingApproval {
    pub(crate) fn from_transcript(transcript: &[SessionMessageEntry]) -> Option<Self> {
        pending_approval_tool(transcript)
            .map(|(call, gate)| PendingApproval::Gate {
                call,
                id: gate.id,
                note: match &gate.state {
                    holt_doc::parts::ToolGateState::Pending { note } => note.clone(),
                    _ => None,
                },
            })
            .or_else(|| {
                crate::transcript::question_card::pending_question(transcript)
                    .map(|(card_id, questions)| PendingApproval::Question { card_id, questions })
            })
            .or_else(|| pending_plan_approval(transcript).map(PendingApproval::Plan))
    }

    pub(crate) fn id(&self) -> &str {
        match self {
            PendingApproval::Gate { id, .. } => id,
            PendingApproval::Plan(key) => key,
            PendingApproval::Question { card_id, .. } => card_id,
        }
    }

    pub(crate) fn prompt(&self) -> ApprovalPrompt {
        match self {
            PendingApproval::Gate { call, note, .. } => {
                let mut prompt = gate_prompt(call);
                if is_model_apply(call) {
                    // Every apply asks the user, whatever the mode — no
                    // session exemption exists, so the always-allow row is
                    // not offered, and the stored proposal's summary leads.
                    prompt.options.retain(|option| {
                        !matches!(
                            option.verdict,
                            BarVerdict::Gate(ApprovalVerdict::AlwaysAllow)
                        )
                    });
                    prompt.title = "Apply these catalog changes?".to_string();
                    if let Some(summary) = note {
                        prompt.target = Some(summary.clone());
                    }
                }
                prompt
            }
            PendingApproval::Plan(_) => plan_prompt(),
            // At open the bar starts on the first question.
            PendingApproval::Question { questions, .. } => question_prompt(questions, 0),
        }
    }
}

/// The bar's cursor: rows `0..options.len()` are the verdict options; row
/// `options.len()` is the trailing note input. The row count rides the
/// prompt (rebuilt per frame), not this struct — a question card's page
/// changes it mid-life.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalBar {
    pub id: String,
    pub kind: BarKind,
    pub selection: usize,
    /// The question card's current page (0-based); 0 on other kinds.
    pub page: usize,
    /// The stashed answer per question page — `None` until that page is
    /// answered. Pages may be answered in any order (←/→ navigate); the
    /// card submits once every slot is filled.
    pub answers: Vec<Option<String>>,
}

impl ApprovalBar {
    pub(crate) fn new(id: String, kind: BarKind, pages: usize) -> Self {
        Self {
            id,
            kind,
            selection: 0,
            page: 0,
            answers: vec![None; pages],
        }
    }

    /// The note row's cursor position: `rows` is the prompt's option
    /// count plus the note row.
    pub(crate) fn note_row(&self, rows: usize) -> usize {
        rows - 1
    }

    pub(crate) fn move_by(&mut self, delta: isize, rows: usize) {
        let next = self.selection as isize + delta;
        self.selection = next.clamp(0, rows as isize - 1) as usize;
    }

    /// A bare digit jumps 1..=rows; out of range is ignored.
    pub(crate) fn press_number(&mut self, number: usize, rows: usize) -> bool {
        if number == 0 || number > rows {
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
                let (call, _gate) = pending_approval_tool(&self.state.read(cx).transcript)?;
                Some(gate_prompt(&call))
            }
            BarKind::Plan => Some(plan_prompt()),
            BarKind::Question => {
                let (_, questions) = crate::transcript::question_card::pending_question(
                    &self.state.read(cx).transcript,
                )?;
                let page = self.approval_bar.as_ref().map_or(0, |bar| bar.page);
                Some(question_prompt(&questions, page))
            }
        }
    }

    /// Confirm the cursor row: an option resolves with its verdict; the
    /// note row hands the shared input focus for the note.
    pub(super) fn approval_bar_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.approval_bar.clone() else {
            return;
        };
        let Some(prompt) = self.bar_prompt(cx) else {
            return;
        };
        let rows = prompt.options.len() + 1;
        if bar.selection == bar.note_row(rows) {
            let handle = self.input.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            return;
        }
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
        // A multi-question card stashes the page's answer into its slot
        // and opens the next UNANSWERED question; the card submits only
        // once every slot is filled — ←/→ may revisit pages freely until
        // then. The submitting verdict carries nothing itself.
        if let BarVerdict::Question(answer) = &verdict {
            let Some(bar) = self.approval_bar.as_mut() else {
                return;
            };
            if let Some(slot) = bar.answers.get_mut(bar.page) {
                *slot = Some(answer.clone());
            }
            if !bar.answers.iter().all(Option::is_some) {
                let page = bar.page;
                let next = (page + 1..bar.answers.len())
                    .find(|&i| bar.answers[i].is_none())
                    .or((0..page).find(|&i| bar.answers[i].is_none()));
                if let Some(target) = next {
                    bar.page = target;
                    bar.selection = 0;
                    self.input.update(cx, |input, cx| input.set_text("", cx));
                    cx.notify();
                    return;
                }
            }
            // Every slot filled — fall through and submit everything.
        }
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
            BarVerdict::Question(_) => {
                let answers = bar
                    .answers
                    .into_iter()
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default();
                crate::transcript::question_card::resolve_question(
                    &self.state,
                    bar.id,
                    answers,
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// ←/→ steps between the question card's pages while any question is
    /// unanswered (the card submits the moment all are). Re-entering an
    /// answered page restores its answer as the cursor: the matching
    /// option row, or the text back into the note input.
    pub(super) fn approval_bar_step_page(&mut self, delta: isize, cx: &mut Context<Self>) {
        let total = self
            .bar_prompt(cx)
            .and_then(|prompt| prompt.pager)
            .map_or(1, |(_, total)| total);
        let target = {
            let Some(bar) = self.approval_bar.as_ref() else {
                return;
            };
            let next = bar.page as isize + delta;
            if next < 0 || next as usize >= total {
                return;
            }
            next as usize
        };
        let stashed = self
            .approval_bar
            .as_ref()
            .and_then(|bar| bar.answers.get(target))
            .cloned()
            .flatten();
        if let Some(bar) = self.approval_bar.as_mut() {
            bar.page = target;
            bar.selection = 0;
        }
        let prompt = self.bar_prompt(cx);
        let restored = stashed.as_ref().and_then(|answer| {
            prompt.as_ref().and_then(|prompt| {
                prompt.options.iter().position(|option| {
                    matches!(&option.verdict, BarVerdict::Question(text) if text == answer)
                })
            })
        });
        if let Some(bar) = self.approval_bar.as_mut() {
            bar.selection = restored.unwrap_or(0);
        }
        match stashed {
            Some(text) if restored.is_none() => {
                let text = text.clone();
                self.input.update(cx, |input, cx| input.set_text(&text, cx));
            }
            _ => self.input.update(cx, |input, cx| input.set_text("", cx)),
        }
        cx.notify();
    }

    /// Close the question bar without answering (Escape): the card is
    /// stamped superseded — persistent, so a restart does not resurrect
    /// the bar — and the suppression holds until the doc catches up.
    pub(super) fn dismiss_approval_bar(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.approval_bar.take() else {
            return;
        };
        self.answered_approvals.insert(bar.id.clone());
        self.input.update(cx, |input, cx| {
            input.set_text("", cx);
            input.set_placeholder("Do anything…", cx);
        });
        crate::transcript::question_card::dismiss_question(&self.state, bar.id, cx);
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
            BarKind::Question => {
                // A question has no blank answer: an empty note keeps the
                // bar up. The non-empty note IS the answer.
                if let Some(answer) = note {
                    self.resolve_approval_bar(BarVerdict::Question(answer), None, cx);
                }
            }
        }
    }

    /// Keys on the bar's root. The bar's own focus handle owns the
    /// keyboard by default (stamped on open), so arrows/Enter/digits land
    /// here instead of in the shared input's caret bindings. A focused
    /// note input owns its keys instead: arrows are caret movement, Enter
    /// is the input's Submit (the note), digits are text. Escape
    /// dismisses a question card and bubbles past the other kinds (see
    /// the module docs).
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
        let kind = self.approval_bar.as_ref().map(|bar| bar.kind);
        if key == "up" || key == "down" {
            let rows = self
                .bar_prompt(cx)
                .map_or(1, |prompt| prompt.options.len() + 1);
            if let Some(bar) = self.approval_bar.as_mut() {
                bar.move_by(if key == "up" { -1 } else { 1 }, rows);
            }
            cx.stop_propagation();
            cx.notify();
        } else if key == "left" || key == "right" {
            // Page navigation for a multi-question card (a focused note
            // input keeps its caret bindings — the early return above).
            self.approval_bar_step_page(if key == "left" { -1 } else { 1 }, cx);
            cx.stop_propagation();
        } else if key == "escape" && kind == Some(BarKind::Question) {
            // A question blocks nothing — the Turn already stopped — so
            // Escape dismisses it instead of bubbling to the (no-op)
            // interrupt. The gate keeps bubbling: its Escape interrupts.
            self.dismiss_approval_bar(cx);
            cx.stop_propagation();
        } else if let Ok(digit) = key.parse::<usize>()
            && (1..=9).contains(&digit)
            && !event.keystroke.modifiers.modified()
        {
            let rows = self
                .bar_prompt(cx)
                .map_or(1, |prompt| prompt.options.len() + 1);
            let Some(bar) = self.approval_bar.as_mut() else {
                return;
            };
            if bar.press_number(digit, rows) {
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
            BarKind::Plan | BarKind::Question => None,
        };
        let rows = prompt.options.len() + 1;
        let selection = bar.selection.min(bar.note_row(rows));
        let note_row = bar.note_row(rows);
        let input_focused = self.input.read(cx).focus_handle.is_focused(window);
        let note_selected = selection == note_row && !input_focused;

        let number_chip = |ix: usize, selected: bool, check: bool| {
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
                // An answered option's chip shows the check in place of
                // its number — the row's answer marker.
                .child(SharedString::from(if check {
                    "✓".to_string()
                } else {
                    format!("{}", ix + 1)
                }))
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

        // A question card's answered page marks its stashed option with
        // a check — the persistent answer, distinct from the keyboard
        // cursor's highlight.
        let stashed = match prompt.kind {
            BarKind::Question => bar.answers.get(bar.page).cloned().flatten(),
            BarKind::Gate | BarKind::Plan => None,
        };
        let options = prompt.options.iter().enumerate().map(|(ix, option)| {
            let selected = ix == selection;
            let verdict = option.verdict.clone();
            let chosen = matches!(
                (&option.verdict, &stashed),
                (BarVerdict::Question(text), Some(answer)) if text == answer
            );
            row_frame(selected, format!("approval-bar-option-{ix}"))
                .id(("approval-bar-option", ix))
                .on_hover(motion::hover_listener(format!("approval-bar-option-{ix}")))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.resolve_approval_bar(verdict.clone(), None, cx)
                }))
                .child(number_chip(ix, selected, chosen))
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
                        .child(option.label.clone()),
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
            .child(number_chip(note_row, note_selected, false))
            .child(div().flex_1().min_w_0().child(self.input.clone()));

        // The frosted wrapper (the composer pill's own chrome): the
        // translucent fill needs the backdrop blur — without it the
        // transcript rows scrolling behind the tall panel bleed through
        // the text.
        Some(
            crate::frost::frosted(
                26.0,
                16.0,
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
                            // The question card's pager: which of the agent's
                            // questions this page answers, flanked by click
                            // affordances for the ←/→ keys.
                            .when_some(prompt.pager, |el, (page, total)| {
                                let page_arrow =
                                    |glyph: &'static str,
                                     enabled: bool,
                                     delta: isize,
                                     key: &'static str| {
                                        div()
                                            .id(SharedString::from(format!(
                                                "approval-bar-page-{key}"
                                            )))
                                            .when(enabled, |el| {
                                                el.cursor_pointer().on_click(cx.listener(
                                                    move |this, _, _, cx| {
                                                        this.approval_bar_step_page(delta, cx);
                                                    },
                                                ))
                                            })
                                            .flex_none()
                                            .px(px(4.0))
                                            .text_size(crate::typography::ui_rems(13.0))
                                            .line_height(px(14.0))
                                            .text_color(if enabled {
                                                theme.text_muted.opacity(0.9)
                                            } else {
                                                theme.text_muted.opacity(0.3)
                                            })
                                            .child(glyph)
                                    };
                                el.child(
                                    div()
                                        .mt(px(2.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(2.0))
                                        .child(page_arrow("‹", page > 1, -1, "left"))
                                        .child(
                                            div()
                                                .text_size(crate::typography::ui_rems(11.0))
                                                .text_color(theme.text_muted.opacity(0.7))
                                                .child(SharedString::from(format!(
                                                    "{page}/{total}"
                                                ))),
                                        )
                                        .child(page_arrow("›", page < total, 1, "right")),
                                )
                            })
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
                            )
                            // The dismiss affordance: nothing is blocked,
                            // so Escape closes the card unanswered — the
                            // running strip's keycap idiom. The gate's Esc
                            // interrupts instead and gets no hint here.
                            .when(prompt.kind == BarKind::Question, |el| {
                                el.child(
                                    div()
                                        .mt(px(6.0))
                                        .flex()
                                        .justify_end()
                                        .items_center()
                                        .gap(px(3.0))
                                        .text_color(theme.text_faint)
                                        .child(
                                            div()
                                                .px(px(3.0))
                                                .rounded(px(4.0))
                                                .border_1()
                                                .border_color(theme.hairline(0.14))
                                                .font_family(theme.font_mono.clone())
                                                .text_size(px(10.0))
                                                .line_height(px(14.0))
                                                .child("Esc"),
                                        )
                                        .child(" to dismiss"),
                                )
                            }),
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
    fn a_forced_model_apply_prompt_titles_the_action_and_leads_with_the_summary() {
        let entry = SessionMessageEntry {
            id: "entry-1".into(),
            role: holt_doc::MessageRole::Assistant,
            parts: vec![holt_doc::MessagePart::Tool {
                id: "call-1".into(),
                call: ToolCall::Unknown {
                    name: "model_apply".into(),
                    input: Some(serde_json::json!({ "proposalId": "p1" })),
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
                subagent_usage: None,
                gate: Some(holt_doc::parts::ToolGate {
                    origin: None,
                    id: "approval-1".into(),
                    state: holt_doc::parts::ToolGateState::Pending {
                        note: Some("Apply 1 catalog change for openai".into()),
                    },
                }),
            }],
            created_at: 0,
            device_id: "device".into(),
            status: None,
            continuation_of: None,
        };
        let pending = PendingApproval::from_transcript(&[entry]).expect("pending gate");
        let prompt = pending.prompt();
        assert_eq!(prompt.title, "Apply these catalog changes?");
        assert_eq!(
            prompt.target.as_deref(),
            Some("Apply 1 catalog change for openai")
        );
        // Allow and deny only: always-allow records nothing for a forced
        // tool, so the row is not offered at all.
        assert_eq!(prompt.options.len(), 2);
        assert!(!prompt.options.iter().any(|option| matches!(
            option.verdict,
            BarVerdict::Gate(ApprovalVerdict::AlwaysAllow)
        )));

        // An ordinary unknown tool keeps the generic shape.
        let ordinary = ToolCall::Unknown {
            name: "something_else".into(),
            input: None,
        };
        assert_eq!(gate_prompt(&ordinary).title, "Allow this action?");
        assert_eq!(gate_prompt(&ordinary).options.len(), 3);
    }

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
        assert_eq!(prompt.note_placeholder, "Enter feedback…");
        let verdicts: Vec<BarVerdict> = prompt
            .options
            .iter()
            .map(|option| option.verdict.clone())
            .collect();
        assert_eq!(verdicts, vec![BarVerdict::Plan("approve")]);
        assert_eq!(prompt.options[0].label.as_ref(), "Approve");
        assert!(!prompt.options[0].danger);
    }

    #[test]
    fn the_cursor_clamps_jumps_and_finds_the_note_row() {
        let mut bar = ApprovalBar::new("g1".into(), BarKind::Gate, 1);
        let rows = 4; // three options + the note row
        assert_eq!(bar.note_row(rows), 3);
        assert_eq!(bar.selection, 0);
        bar.move_by(-1, rows);
        assert_eq!(bar.selection, 0, "clamped at the top");
        bar.move_by(10, rows);
        assert_eq!(bar.selection, 3, "clamped at the note row");
        assert!(bar.press_number(2, rows));
        assert_eq!(bar.selection, 1);
        assert!(bar.press_number(4, rows));
        assert_eq!(bar.selection, 3);
        assert!(!bar.press_number(5, rows), "out of range ignored");
        assert!(!bar.press_number(0, rows));
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
                subagent_usage: None,
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
    fn the_pending_scan_prefers_gate_then_question_then_plan() {
        use holt_doc::{GateVerdict, PlanApprovalVerdict, ToolGateState};
        let question = |state: holt_doc::ChoiceCardState| SessionMessageEntry {
            id: "q-entry".into(),
            role: holt_doc::MessageRole::Assistant,
            parts: vec![holt_doc::MessagePart::QuestionCard {
                id: "q1".into(),
                questions: vec![
                    holt_doc::CardQuestion {
                        question: "Prefix or suffix?".into(),
                        options: vec!["prefix".into(), "suffix".into()],
                    },
                    holt_doc::CardQuestion {
                        question: "Which store?".into(),
                        options: vec!["memory".into(), "sqlite".into()],
                    },
                ],
                answers: Vec::new(),
                state,
            }],
            created_at: 2,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        };
        // Gate and question both pending: the gate wins the tie.
        let gate_and_question = vec![
            question(holt_doc::ChoiceCardState::Pending),
            gated(ToolGateState::Pending { note: None }),
        ];
        let Some(PendingApproval::Gate { id, .. }) =
            PendingApproval::from_transcript(&gate_and_question)
        else {
            panic!("expected the gate to win")
        };
        assert_eq!(id, "g1");
        // Question only.
        let question_only = vec![question(holt_doc::ChoiceCardState::Pending)];
        let Some(PendingApproval::Question {
            card_id, questions, ..
        }) = PendingApproval::from_transcript(&question_only)
        else {
            panic!("expected the question")
        };
        assert_eq!(card_id, "q1");
        assert_eq!(questions.len(), 2);
        assert_eq!(
            questions[0].options,
            vec!["prefix".to_string(), "suffix".to_string()]
        );
        // Question over plan.
        let question_and_plan = vec![
            plan_pending(holt_doc::PlanApprovalState::Pending),
            question(holt_doc::ChoiceCardState::Pending),
        ];
        assert!(matches!(
            PendingApproval::from_transcript(&question_and_plan),
            Some(PendingApproval::Question { .. })
        ));
        // Nothing pending.
        let settled = vec![
            plan_pending(holt_doc::PlanApprovalState::Settled {
                verdict: PlanApprovalVerdict::Approved,
            }),
            question(holt_doc::ChoiceCardState::Chosen),
            gated(ToolGateState::Settled {
                verdict: GateVerdict::Allowed,
            }),
        ];
        assert!(PendingApproval::from_transcript(&settled).is_none());
    }

    #[test]
    fn the_question_prompt_serves_one_page_of_the_card() {
        let questions = vec![
            holt_doc::CardQuestion {
                question: "Prefix or suffix?".into(),
                options: vec!["prefix".into(), "suffix".into()],
            },
            holt_doc::CardQuestion {
                question: "Which store?".into(),
                options: vec!["memory".into(), "sqlite".into()],
            },
        ];
        // The first page carries the first question and the pager.
        let prompt = question_prompt(&questions, 0);
        assert_eq!(prompt.kind, BarKind::Question);
        assert_eq!(prompt.title, "Prefix or suffix?");
        assert_eq!(prompt.target, None);
        assert_eq!(prompt.note_placeholder, "Answer in words…");
        assert_eq!(prompt.pager, Some((1, 2)));
        assert_eq!(
            prompt.options,
            vec![
                ApprovalOption {
                    label: "prefix".into(),
                    verdict: BarVerdict::Question("prefix".into()),
                    danger: false,
                },
                ApprovalOption {
                    label: "suffix".into(),
                    verdict: BarVerdict::Question("suffix".into()),
                    danger: false,
                },
            ]
        );
        // The second page swaps the title, options, and pager.
        let prompt = question_prompt(&questions, 1);
        assert_eq!(prompt.title, "Which store?");
        assert_eq!(prompt.pager, Some((2, 2)));
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(prompt.options[0].label.as_ref(), "memory");
        // A single question carries no pager.
        let single = &questions[..1];
        assert_eq!(question_prompt(single, 0).pager, None);
        // An out-of-range page clamps to the last question.
        assert_eq!(question_prompt(&questions, 9).title, "Which store?");
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
            s.transcript
                .push(gated(ToolGateState::Pending { note: None }));
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
            let mut next = gated(ToolGateState::Pending { note: None });
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

    /// The question producer: a two-question card, so the bar's paging
    /// shows. The empty note keeps a page up; an answer advances or, on
    /// the last page, submits everything.
    fn question_pending(state: holt_doc::ChoiceCardState) -> holt_doc::SessionMessageEntry {
        holt_doc::SessionMessageEntry {
            id: "q-entry".into(),
            role: holt_doc::MessageRole::Assistant,
            parts: vec![holt_doc::MessagePart::QuestionCard {
                id: "q1".into(),
                questions: vec![
                    holt_doc::CardQuestion {
                        question: "Prefix or suffix?".into(),
                        options: vec!["prefix".into(), "suffix".into()],
                    },
                    holt_doc::CardQuestion {
                        question: "Which store?".into(),
                        options: vec!["memory".into(), "sqlite".into()],
                    },
                ],
                answers: Vec::new(),
                state,
            }],
            created_at: 2,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[gpui::test]
    fn the_bar_pages_through_a_multi_question_card(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));

        state.update(cx, |s, cx| {
            s.transcript
                .push(question_pending(holt_doc::ChoiceCardState::Pending));
            cx.notify();
        });
        composer.update(cx, |this, cx| {
            let bar = this.approval_bar.as_ref().expect("the bar opened");
            assert_eq!(bar.id, "q1");
            assert_eq!(bar.kind, BarKind::Question);
            // Page one carries the first question.
            let prompt = this.bar_prompt(cx).unwrap();
            assert_eq!(prompt.title, "Prefix or suffix?");
            assert_eq!(prompt.pager, Some((1, 2)));
            assert_eq!(prompt.options.len(), 2);
        });
        // Draw the question bar (pager, options, note row) without
        // panicking.
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| composer.clone().into_any_element(),
        );

        // An empty note is not an answer: the page stays up.
        composer.update(cx, |this, cx| {
            this.on_submit(cx);
            assert!(this.approval_bar.is_some());
        });

        // The first answer advances the page instead of retiring the bar.
        composer.update(cx, |this, cx| {
            this.resolve_approval_bar(
                crate::composer::approval_bar::BarVerdict::Question("suffix".into()),
                None,
                cx,
            );
            let bar = this.approval_bar.as_ref().expect("the bar advanced");
            assert_eq!(bar.page, 1);
            assert_eq!(bar.answers, vec![Some("suffix".to_string()), None]);
            let prompt = this.bar_prompt(cx).unwrap();
            assert_eq!(prompt.title, "Which store?");
            assert_eq!(prompt.pager, Some((2, 2)));
        });

        // ← returns to the answered page and restores its option as the
        // cursor; → returns forward with the stash intact. ← again is a
        // no-op at the first page.
        composer.update(cx, |this, cx| {
            this.approval_bar_step_page(-1, cx);
            let bar = this.approval_bar.as_ref().unwrap();
            assert_eq!(bar.page, 0);
            assert_eq!(bar.selection, 1, "the stashed option becomes the cursor");
            this.approval_bar_step_page(1, cx);
            assert_eq!(this.approval_bar.as_ref().unwrap().page, 1);
            this.approval_bar_step_page(1, cx);
            assert_eq!(this.approval_bar.as_ref().unwrap().page, 1);
        });

        // Re-answering page one restashes; the card stays open while a
        // slot is empty, and the LAST answer submits everything.
        composer.update(cx, |this, cx| {
            this.approval_bar_step_page(-1, cx);
            assert_eq!(this.approval_bar.as_ref().unwrap().page, 0);
            this.resolve_approval_bar(
                crate::composer::approval_bar::BarVerdict::Question("prefix".into()),
                None,
                cx,
            );
            // The only unanswered slot is page one — answering lands there.
            assert_eq!(this.approval_bar.as_ref().unwrap().page, 1);
            let bar = this.approval_bar.as_ref().unwrap();
            assert_eq!(
                bar.answers,
                vec![Some("prefix".to_string()), None],
                "the restash replaced the page-one answer"
            );
        });

        // The final typed answer fills the card: the bar retires under
        // suppression until the doc stamps the card.
        composer.update(cx, |this, cx| {
            this.input
                .update(cx, |input, cx| input.set_text("sqlite", cx));
            this.on_submit(cx);
            assert!(this.approval_bar.is_none());
            assert!(this.answered_approvals.contains("q1"));
        });
        // The settle re-arms the surface.
        state.update(cx, |s, cx| {
            s.transcript[0] = question_pending(holt_doc::ChoiceCardState::Chosen);
            cx.notify();
        });
        composer.update(cx, |this, _| {
            assert!(!this.answered_approvals.contains("q1"));
        });
    }

    /// Escape dismisses the unanswered card: the bar retires under
    /// suppression, and the superseded stamp keeps it closed.
    #[gpui::test]
    fn escape_dismisses_a_pending_question_bar(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let (composer, cx) = cx.add_window_view(|_window, cx| Composer::new(state.clone(), cx));

        state.update(cx, |s, cx| {
            s.transcript
                .push(question_pending(holt_doc::ChoiceCardState::Pending));
            cx.notify();
        });
        cx.run_until_parked();
        composer.update(cx, |this, _| {
            assert!(this.approval_bar.is_some());
        });

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        composer.update(cx, |this, _| {
            assert!(this.approval_bar.is_none());
            assert!(this.answered_approvals.contains("q1"));
        });

        // The superseded stamp keeps the bar down across frames.
        state.update(cx, |s, cx| {
            s.transcript[0] = question_pending(holt_doc::ChoiceCardState::Superseded);
            cx.notify();
        });
        cx.run_until_parked();
        composer.update(cx, |this, _| {
            assert!(this.approval_bar.is_none(), "a dismissed card stays down");
            assert!(!this.answered_approvals.contains("q1"));
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
            s.transcript
                .push(gated(ToolGateState::Pending { note: None }));
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
