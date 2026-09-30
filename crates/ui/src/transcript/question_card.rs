//! The question card (ADR-0040): a question the agent asked through
//! `ask_user`, rendered in the transcript flow where it was asked. While
//! pending it shows the enumerated options plus a free-text input; a click
//! or a submitted line settles the card through `SettleQuestion`, which
//! stamps the pick and queues the answer as an ordinary user message. The
//! card holds no outcome of its own — settled states come back from the
//! engine as doc stamps; only in-flight and error state live here.

use gpui::{AnyElement, Entity, SharedString, Subscription, div, prelude::*, px};

use holt_doc::ChoiceCardState;
use holt_rpc::methods;

use super::Transcript;
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::theme::Theme;

/// Per-card interactive state, keyed by row id on the [`Transcript`].
#[derive(Default)]
pub(super) struct QuestionCardUi {
    busy: bool,
    error: Option<SharedString>,
    input: Option<Entity<ComposerInput>>,
    _input_events: Option<Subscription>,
}

impl Transcript {
    /// Settle a question card on one of its options (the click).
    pub(super) fn settle_question(
        &mut self,
        row_id: SharedString,
        card_id: SharedString,
        answer: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.send_question_answer(row_id, card_id, answer.to_string(), cx);
    }

    /// Submit the typed free-text answer (the input's Submitted event).
    fn submit_question_answer(
        &mut self,
        row_id: SharedString,
        card_id: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(ui) = self.question_cards.get(&row_id) else {
            return;
        };
        let Some(input) = ui.input.clone() else {
            return;
        };
        let answer = input.read(cx).text().trim().to_string();
        if answer.is_empty() {
            return;
        }
        input.update(cx, |input, cx| input.set_text("", cx));
        self.send_question_answer(row_id, card_id, answer, cx);
    }

    fn send_question_answer(
        &mut self,
        row_id: SharedString,
        card_id: SharedString,
        answer: String,
        cx: &mut Context<Self>,
    ) {
        let (Some(chat_id), Some(engine)) =
            (self.chat_id.clone(), self.state.read(cx).engine().cloned())
        else {
            return;
        };
        let ui = self.question_cards.entry(row_id.clone()).or_default();
        if ui.busy {
            return;
        }
        ui.busy = true;
        ui.error = None;
        self.remeasure_row(&row_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SETTLE_QUESTION,
                    serde_json::json!({
                        "chatId": chat_id,
                        "cardId": card_id,
                        "choice": answer,
                    }),
                )
                .await;
            this.update(cx, |this, cx| {
                if let Some(ui) = this.question_cards.get_mut(&row_id) {
                    ui.busy = false;
                    ui.error = result.as_ref().err().map(|error| error.to_string().into());
                }
                this.remeasure_row(&row_id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_question_card(
        &mut self,
        row_id: &SharedString,
        card_id: &SharedString,
        question: &SharedString,
        options: &[SharedString],
        chosen: Option<&SharedString>,
        state: ChoiceCardState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = state == ChoiceCardState::Pending;
        // The input exists only while the card is pending (dropped in
        // `sync` once settled, rendered or not).
        if pending
            && self
                .question_cards
                .get(row_id)
                .is_none_or(|ui| ui.input.is_none())
        {
            let input = cx.new(|cx| ComposerInput::new("Answer in words…", cx));
            let (submit_row, submit_card) = (row_id.clone(), card_id.clone());
            let events = cx.subscribe(&input, move |this, _, event, cx| match event {
                ComposerInputEvent::Edited => cx.notify(),
                ComposerInputEvent::Submitted => {
                    this.submit_question_answer(submit_row.clone(), submit_card.clone(), cx)
                }
                _ => {}
            });
            let ui = self.question_cards.entry(row_id.clone()).or_default();
            ui.input = Some(input);
            ui._input_events = Some(events);
        }
        let ui = self.question_cards.get(row_id);
        let busy = ui.is_some_and(|ui| ui.busy);
        let error = ui.and_then(|ui| ui.error.clone());
        let input = ui.and_then(|ui| ui.input.clone());

        let card = card_frame(theme, pending).child(
            div()
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text)
                .child(question.clone()),
        );
        let card = match state {
            ChoiceCardState::Pending => card
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .when(busy, |list| list.opacity(0.4))
                        .children(options.iter().enumerate().map(|(ix, option)| {
                            let selector = format!("question-card-{row_id}-{ix}");
                            let settle_row = row_id.clone();
                            let settle_card = card_id.clone();
                            let answer = option.clone();
                            div()
                                .id(SharedString::from(selector.clone()))
                                .debug_selector(move || selector.clone())
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .px(px(8.0))
                                .py(px(6.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .cursor_pointer()
                                .hover(|style| style.bg(theme.hairline(0.06)))
                                .child(number_mark(ix, theme))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(crate::typography::ui_rems(12.5))
                                        .text_color(theme.text)
                                        .child(option.clone()),
                                )
                                .when(!busy, |row| {
                                    row.on_click(cx.listener(move |this, _, _, cx| {
                                        this.settle_question(
                                            settle_row.clone(),
                                            settle_card.clone(),
                                            answer.clone(),
                                            cx,
                                        )
                                    }))
                                })
                        })),
                )
                .when_some(input, |card, input| {
                    card.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .child(input),
                    )
                }),
            ChoiceCardState::Chosen => card.child(state_line(
                format!(
                    "✓ Answered · {}",
                    chosen.map(SharedString::as_ref).unwrap_or_default()
                ),
                theme,
            )),
            ChoiceCardState::Superseded => {
                card.child(state_line("No longer active — answered in chat", theme))
            }
        };
        div()
            .py(px(4.0))
            .w_full()
            .child(card.children(error.map(|error| {
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.danger)
                    .child(error)
            })))
            .into_any_element()
    }
}

fn number_mark(ix: usize, _theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .size(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .bg(crate::theme::ink(0.05))
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(crate::theme::ink(0.62))
        .child(SharedString::from(format!("{}", ix + 1)))
}

fn card_frame(theme: &Theme, active: bool) -> gpui::Div {
    div()
        .w_full()
        .max_w(px(720.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(10.0))
        .rounded(px(12.0))
        .border_1()
        .when(active, |card| {
            card.bg(theme.accent.opacity(0.07))
                .border_color(theme.accent.opacity(0.45))
        })
        .when(!active, |card| card.border_color(theme.hairline(0.12)))
}

fn state_line(text: impl Into<SharedString>, theme: &Theme) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(11.5))
        .text_color(theme.text_muted)
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::transcript::model::RowKind;
    use holt_doc::{MessagePart, MessageRole, SessionMessageEntry};

    fn question_entry(state: ChoiceCardState, chosen: Option<&str>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::QuestionCard {
                id: "q1".into(),
                question: "Prefix or suffix?".into(),
                options: vec!["prefix".into(), "suffix".into()],
                chosen: chosen.map(str::to_owned),
                state,
            }],
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn rows(entry: SessionMessageEntry) -> Vec<crate::transcript::model::Row> {
        let mut parse = |_: &str, text: &str| {
            std::sync::Arc::new(crate::markdown::parser::parse_full(text))
                as std::sync::Arc<crate::markdown::parser::BlockTree>
        };
        crate::transcript::rows_for_entry(&entry, false, &mut parse)
    }

    #[test]
    fn a_question_card_folds_into_one_row_keyed_by_part_id() {
        let rows = rows(question_entry(ChoiceCardState::Pending, None));
        assert_eq!(rows.len(), 1);
        let RowKind::QuestionCard {
            card_id,
            question,
            options,
            chosen,
            state,
        } = &rows[0].kind
        else {
            panic!("expected a question card row");
        };
        assert_eq!(card_id.as_ref(), "q1");
        assert_eq!(question.as_ref(), "Prefix or suffix?");
        assert_eq!(options.len(), 2);
        assert!(chosen.is_none());
        assert_eq!(*state, ChoiceCardState::Pending);
    }

    #[gpui::test]
    fn the_pending_card_mounts_an_input_and_settles_through_the_rpc(cx: &mut gpui::TestAppContext) {
        use crate::transcript::Transcript;

        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| AppState::new());
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));

        state.update(cx, |s, cx| {
            s.transcript
                .push(question_entry(ChoiceCardState::Pending, None));
            cx.notify();
        });
        // Draw the pending card (question, options, free-text input).
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| transcript.clone().into_any_element(),
        );
        transcript.update(cx, |this, _| {
            assert!(
                this.question_cards.values().all(|ui| ui.input.is_some()),
                "a pending card owns its free-text input"
            );
        });

        // The settle stamps the card in the doc; the settled card drops
        // its interactive state on the next sync.
        state.update(cx, |s, cx| {
            s.transcript[0] = question_entry(ChoiceCardState::Chosen, Some("suffix"));
            cx.notify();
        });
        cx.run_until_parked();
        transcript.update(cx, |this, _| {
            assert!(
                this.question_cards.values().all(|ui| ui.input.is_none()),
                "a settled card drops its input"
            );
        });
    }
}
