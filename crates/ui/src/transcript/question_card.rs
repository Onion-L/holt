//! The question card (ADR-0040): a question the agent asked through
//! `ask_user`, answered in the composer's approval bar (its third kind —
//! the transcript builds no interactive counterpart, the gate's rule).
//! While pending the card scans out of the transcript into the bar; once
//! settled it renders as a small marker row carrying the stamped answer.
//! `resolve_question` is the bar's verdict channel.

use gpui::{AnyElement, App, SharedString, div, prelude::*, px};

use holt_doc::{ChoiceCardState, MessagePart, SessionMessageEntry};
use holt_rpc::methods;

use crate::state::AppState;
use crate::theme::Theme;

/// The latest still-pending question card: the composer approval bar's
/// data source — `(card id, question, options)` in card order.
pub fn pending_question(
    transcript: &[SessionMessageEntry],
) -> Option<(String, String, Vec<String>)> {
    transcript
        .iter()
        .rev()
        .flat_map(|entry| entry.parts.iter().rev())
        .find_map(|part| match part {
            MessagePart::QuestionCard {
                id,
                question,
                options,
                state: ChoiceCardState::Pending,
                ..
            } => Some((id.clone(), question.clone(), options.clone())),
            _ => None,
        })
}

/// Send the answer (fire-and-forget: failures are no-ops engine-side,
/// and the doc's stamped card is what settles the UI — the gate's
/// channel contract). The approval bar's question channel.
pub fn resolve_question(
    state: &gpui::Entity<AppState>,
    card_id: String,
    answer: String,
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
                serde_json::json!({ "chatId": chat_id, "cardId": card_id, "choice": answer }),
            )
            .await
        {
            tracing::warn!(error = %err, "SettleQuestion failed");
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

/// The settled card's compact marker row. A pending card builds no row —
/// the bar carries it — so this renders only settled states; a pending
/// card that still reaches render (a stale frame) shows its bare
/// question without affordances.
pub(super) fn render_question_card(
    question: &SharedString,
    chosen: Option<&SharedString>,
    state: ChoiceCardState,
    theme: &Theme,
) -> AnyElement {
    let line = match state {
        ChoiceCardState::Chosen => state_line(
            format!(
                "✓ Answered · {}",
                chosen.map(SharedString::as_ref).unwrap_or_default()
            ),
            theme,
        ),
        ChoiceCardState::Superseded => state_line("No longer active — answered in chat", theme),
        ChoiceCardState::Pending => state_line("Waiting for your answer…", theme),
    };
    div()
        .py(px(4.0))
        .w_full()
        .child(card_frame(theme).child(question.clone()).child(line))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question_part(state: ChoiceCardState) -> MessagePart {
        MessagePart::QuestionCard {
            id: "q1".into(),
            question: "Prefix or suffix?".into(),
            options: vec!["prefix".into(), "suffix".into()],
            chosen: None,
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
        let (id, question, options) =
            pending_question(&[entry(vec![question_part(ChoiceCardState::Pending)])]).unwrap();
        assert_eq!(id, "q1");
        assert_eq!(question, "Prefix or suffix?");
        assert_eq!(options, vec!["prefix".to_string(), "suffix".to_string()]);

        // Settled cards never report.
        assert!(pending_question(&[entry(vec![question_part(ChoiceCardState::Chosen)])]).is_none());
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
            question_part(ChoiceCardState::Pending),
        ]);
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            rows[0].kind,
            RowKind::ToolGroup {
                auto_open: true,
                ..
            }
        ));
        assert!(matches!(
            rows[0].kind,
            RowKind::ToolGroup {
                auto_open: true,
                ..
            }
        ));

        let rows = rows_for(vec![
            ask_user_tool(),
            MessagePart::QuestionCard {
                id: "q1".into(),
                question: "Prefix or suffix?".into(),
                options: vec!["prefix".into(), "suffix".into()],
                chosen: Some("suffix".into()),
                state: ChoiceCardState::Chosen,
            },
        ]);
        assert_eq!(rows.len(), 2);
        let RowKind::QuestionCard {
            question,
            chosen,
            state,
        } = &rows[1].kind
        else {
            panic!("expected the marker row");
        };
        assert_eq!(question.as_ref(), "Prefix or suffix?");
        assert_eq!(chosen.as_deref(), Some("suffix"));
        assert_eq!(*state, ChoiceCardState::Chosen);
    }

    #[gpui::test]
    fn the_marker_renders_in_every_settled_state(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let theme = Theme::default();
        for (state, chosen) in [
            (ChoiceCardState::Chosen, Some(SharedString::from("suffix"))),
            (ChoiceCardState::Superseded, None),
        ] {
            let element =
                render_question_card(&"Prefix or suffix?".into(), chosen.as_ref(), state, &theme);
            cx.draw(
                gpui::point(gpui::px(0.0), gpui::px(0.0)),
                gpui::size(gpui::px(800.0), gpui::px(600.0)),
                |_, _| element,
            );
        }
    }
}
