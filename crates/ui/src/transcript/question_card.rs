//! The question card (ADR-0040): 1–4 questions the agent asked through
//! `ask_user`, answered in the composer's approval bar (its third kind —
//! the transcript builds no interactive counterpart, the gate's rule).
//! While pending the card scans out of the transcript into the bar, one
//! page per question; once settled it renders as a small marker row
//! carrying the stamped answers. `resolve_question` is the bar's verdict
//! channel.

use gpui::{AnyElement, App, SharedString, div, prelude::*, px};

use holt_doc::{CardQuestion, ChoiceCardState, MessagePart, SessionMessageEntry};
use holt_rpc::methods;

use crate::state::AppState;
use crate::theme::Theme;

/// The latest still-pending question card: the composer approval bar's
/// data source — the card id and its questions, in card order.
pub fn pending_question(transcript: &[SessionMessageEntry]) -> Option<(String, Vec<CardQuestion>)> {
    transcript
        .iter()
        .rev()
        .flat_map(|entry| entry.parts.iter().rev())
        .find_map(|part| match part {
            MessagePart::QuestionCard {
                id,
                questions,
                state: ChoiceCardState::Pending,
                ..
            } => Some((id.clone(), questions.clone())),
            _ => None,
        })
}

/// Send the answers (fire-and-forget: failures are no-ops engine-side,
/// and the doc's stamped card is what settles the UI — the gate's
/// channel contract). One call per card, every question answered. The
/// approval bar's question channel.
pub fn resolve_question(
    state: &gpui::Entity<AppState>,
    card_id: String,
    answers: Vec<String>,
    cx: &mut App,
) {
    let (engine, chat_id) = {
        let state = state.read(cx);
        (state.engine().cloned(), state.selected_chat.clone())
    };
    let (Some(engine), Some(chat_id)) = (engine, chat_id) else {
        return;
    };
    cx.spawn(async move |_| {
        if let Err(err) = engine
            .client()
            .call(
                methods::SETTLE_QUESTION,
                serde_json::json!({ "chatId": chat_id, "cardId": card_id, "choices": answers }),
            )
            .await
        {
            tracing::warn!(error = %err, "SettleQuestion failed");
        }
    })
    .detach();
}

/// Dismiss the pending card without answering (the bar's Escape):
/// fire-and-forget; the doc's Superseded stamp is what settles the UI.
pub fn dismiss_question(state: &gpui::Entity<AppState>, card_id: String, cx: &mut App) {
    let (engine, chat_id) = {
        let state = state.read(cx);
        (state.engine().cloned(), state.selected_chat.clone())
    };
    let (Some(engine), Some(chat_id)) = (engine, chat_id) else {
        return;
    };
    cx.spawn(async move |_| {
        if let Err(err) = engine
            .client()
            .call(
                methods::DISMISS_QUESTION,
                serde_json::json!({ "chatId": chat_id, "cardId": card_id }),
            )
            .await
        {
            tracing::warn!(error = %err, "DismissQuestion failed");
        }
    })
    .detach();
}

fn card_frame(theme: &Theme) -> gpui::Div {
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
        .border_color(theme.hairline(0.12))
}

fn state_line(text: impl Into<SharedString>, theme: &Theme) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(11.5))
        .text_color(theme.text_muted)
        .child(text.into())
}

/// The settled card's compact marker row: one line per question. A
/// pending card builds no row — the bar carries it — so this renders
/// only settled states; a pending card that still reaches render (a
/// stale frame) shows its bare questions without affordances. A
/// superseded card is dead history: one truncated muted line, no frame.
pub(super) fn render_question_card(
    questions: &[SharedString],
    answers: &[SharedString],
    state: ChoiceCardState,
    theme: &Theme,
) -> AnyElement {
    let mut card = card_frame(theme);
    match state {
        ChoiceCardState::Chosen => {
            for (question, answer) in questions.iter().zip(answers) {
                card = card.child(state_line(
                    format!("✓ Answered · {question} → {answer}"),
                    theme,
                ));
            }
        }
        ChoiceCardState::Superseded => {
            let summary = questions
                .iter()
                .map(SharedString::as_ref)
                .collect::<Vec<_>>()
                .join(" · ");
            return div()
                .py(px(4.0))
                .w_full()
                .child(state_line(format!("{summary} — no longer active"), theme).truncate())
                .into_any_element();
        }
        ChoiceCardState::Pending => {
            for question in questions {
                card = card.child(state_line(question.clone(), theme));
            }
            card = card.child(state_line("Waiting for your answer…", theme));
        }
    }
    div().py(px(4.0)).w_full().child(card).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_questions() -> Vec<CardQuestion> {
        vec![
            CardQuestion {
                question: "Prefix or suffix?".into(),
                options: vec!["prefix".into(), "suffix".into()],
            },
            CardQuestion {
                question: "Which store?".into(),
                options: vec!["memory".into(), "sqlite".into()],
            },
        ]
    }

    fn question_part(state: ChoiceCardState, answers: Vec<String>) -> MessagePart {
        MessagePart::QuestionCard {
            id: "q1".into(),
            questions: two_questions(),
            answers,
            state,
        }
    }

    fn entry(parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "m1".into(),
            role: holt_doc::MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn the_scan_finds_only_the_latest_pending_card() {
        let (id, questions) =
            pending_question(&[entry(vec![question_part(ChoiceCardState::Pending, vec![])])])
                .unwrap();
        assert_eq!(id, "q1");
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].question, "Prefix or suffix?");

        // Settled cards never report.
        assert!(
            pending_question(&[entry(vec![question_part(ChoiceCardState::Chosen, vec![])])])
                .is_none()
        );
        assert!(pending_question(&[]).is_none());
    }

    fn rows_for(parts: Vec<MessagePart>) -> Vec<crate::transcript::model::Row> {
        let mut entry = entry(parts);
        // The live-tail flush keys on streaming, like the gate's test.
        entry.status = Some(holt_doc::MessageStatus::Streaming);
        let mut parse = |_: &str, text: &str| {
            std::sync::Arc::new(crate::markdown::parser::parse_full(text))
                as std::sync::Arc<crate::markdown::parser::BlockTree>
        };
        crate::transcript::rows_for_entry(&entry, false, &mut parse)
    }

    fn ask_user_tool() -> MessagePart {
        MessagePart::Tool {
            id: "t1".into(),
            call: holt_proto::ToolCall::Unknown {
                name: "ask_user".into(),
                input: None,
            },
            is_error: false,
            resolved: true,
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
            gate: None,
        }
    }

    /// A pending card builds no row (the bar carries it) and leaves the
    /// ask_user chip as the tail group; the settle lands the marker row.
    #[test]
    fn pending_builds_no_row_and_the_settle_lands_the_marker() {
        use crate::transcript::model::RowKind;

        let rows = rows_for(vec![
            ask_user_tool(),
            question_part(ChoiceCardState::Pending, vec![]),
        ]);
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            rows[0].kind,
            RowKind::ToolGroup {
                auto_open: true,
                ..
            }
        ));

        let rows = rows_for(vec![
            ask_user_tool(),
            question_part(
                ChoiceCardState::Chosen,
                vec!["suffix".into(), "sqlite".into()],
            ),
        ]);
        assert_eq!(rows.len(), 2);
        let RowKind::QuestionCard {
            questions,
            answers,
            state,
        } = &rows[1].kind
        else {
            panic!("expected the marker row");
        };
        assert_eq!(questions.len(), 2);
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[1].as_ref(), "sqlite");
        assert_eq!(*state, ChoiceCardState::Chosen);
    }

    #[gpui::test]
    fn the_marker_renders_in_every_settled_state(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let theme = Theme::default();
        let questions: Vec<SharedString> = two_questions()
            .iter()
            .map(|q| SharedString::from(q.question.clone()))
            .collect();
        for (state, answers) in [
            (ChoiceCardState::Chosen, vec![SharedString::from("suffix")]),
            (ChoiceCardState::Superseded, vec![]),
        ] {
            let element = render_question_card(&questions, &answers, state, &theme);
            cx.draw(
                gpui::point(gpui::px(0.0), gpui::px(0.0)),
                gpui::size(gpui::px(800.0), gpui::px(600.0)),
                |_, _| element,
            );
        }
    }
}
